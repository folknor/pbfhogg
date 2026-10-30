//! Merge multiple sorted PBF files into one. Equivalent to `osmium merge`.
//!
//! Two-pass blob-level merge through the engine shared with `sort`
//! (`commands::blob_merge`):
//!   Pass 1: Build blob index from all input files
//!   Pass 2: Write in sorted order - passthrough for non-overlapping blobs,
//!           decode + sweep merge with dedup for overlapping blobs.
//!
//! Exact duplicates (same type, ID, version) across input files are removed.
//! Common when merging geographic extracts that share border elements. Only
//! blobs whose ID ranges overlap are ever decoded, so duplicates *within* one
//! blob that overlaps nothing pass through untouched.
//!
//! The output header is reconciled across every input's header by the same
//! owner every command uses (`commands::output_header::build`).

use crate::commands::blob_merge::{self, IndexPolicy, Plan};
use crate::commands::{HeaderOverrides, writer_from_header_bytes};
use crate::error::{Error, Result};
use crate::read::input::{InputSource, PinnedInput};
use crate::write::WriteOptions;

/// Statistics from a multi-PBF merge operation.
///
/// `nodes`, `ways` and `relations` are *observed* output counts: elements the
/// merge decoded and emitted from an overlap run (duplicates it dropped are in
/// `duplicates_removed`, not here), plus passthrough blobs without indexdata,
/// whose count came from decompressing and scanning the payload.
pub struct MergePbfStats {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
    /// Elements in passthrough blobs whose count is the blob's own
    /// `BlobHeader.indexdata` claim, copied through without a decode.
    ///
    /// Kept apart from the observed counts on purpose, as in `sort` and
    /// `getid`: nothing verifies that claim (`check --index` exists to), so
    /// folding it into "Merged N elements" reported hearsay as observation and
    /// a lying indexdata moved N with no way for the reader to tell.
    pub passthrough_claimed: u64,
    pub blobs_passthrough: u64,
    pub blobs_rewritten: u64,
    /// Blobs holding no element, left out of the output.
    pub blobs_omitted: u64,
    /// Every input data blob: passthrough, rewritten and omitted together.
    pub blobs_total: u64,
    pub duplicates_removed: u64,
}

impl MergePbfStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let total = self.nodes + self.ways + self.relations;
        writeln!(
            w,
            "Merged {total} elements: {} nodes, {} ways, {} relations \
             ({} blobs: {} passthrough, {} rewritten{}, {} duplicates removed)",
            self.nodes,
            self.ways,
            self.relations,
            self.blobs_total,
            self.blobs_passthrough,
            self.blobs_rewritten,
            crate::commands::sort::omitted_clause(self.blobs_omitted),
            self.duplicates_removed,
        )?;
        if self.passthrough_claimed > 0 {
            writeln!(
                w,
                "  plus {} elements in blobs copied through without decoding, per \
                 their indexdata (unverified)",
                self.passthrough_claimed,
            )?;
        }

        Ok(())
    }
}

/// Merge multiple sorted PBF files into one, removing exact duplicates
/// (same type, ID and version, an absent version being 0) across inputs.
///
/// All inputs must be sorted (Sort.Type_then_ID), and each must start with
/// exactly one OSMHeader blob. Uses blob-level index for fast passthrough of
/// non-overlapping blobs; overlapping blobs are decoded, merged, and deduped
/// via a streaming sweep. A blob that overlaps no other is never decoded, so
/// duplicates inside it survive.
///
/// Every input is pinned before any is read, as in [`cat`](super::cat), so
/// each is read from the file the run started on; a caller's pin is used as
/// it is. Pass 2 holds one source
/// per argument, so the run asks the host for one descriptor per argument
/// (`ProcessActions::fd_allowance`) and is refused up front when allowed
/// fewer. A caller's pin counts here too: pass 2 opens a new description
/// of it. As with [`cat`](super::cat), the library never raises the open-file
/// limit; the refusal says what the caller can change.
pub fn merge_pbf<'a, S: Into<InputSource<'a>> + Copy>(
    inputs: &[S],
    output: impl crate::output::BindOutput,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<MergePbfStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = merge_pbf_to(inputs, &output, opts, overrides, host)?;
    output.publish()?;
    Ok(result)
}

/// [`merge_pbf`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn merge_pbf_to<'a, S: Into<InputSource<'a>> + Copy>(
    inputs: &[S],
    output: &crate::output::BoundOutput,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<MergePbfStats> {
    let sources: Vec<InputSource<'a>> = inputs.iter().map(|&input| input.into()).collect();
    if sources.is_empty() {
        return Err(Error::invalid_argument("no input files specified"));
    }
    // Every input is opened three times (the indexdata probe, pass 1's
    // index walk, pass 2's source), so every input is pinned up front. The
    // pins are held until pass 2 has opened its sources, one per argument
    // and held for the whole of pass 2, so the run asks for one descriptor
    // per argument, and holds the reservation here, across both passes.
    let _reservation = super::admit_descriptors(
        sources.len(),
        &format!(
            "a deduplicating cat holds a file descriptor per input argument through its second pass: {} \
             input files",
            sources.len()
        ),
        host,
    )?;
    let plan = plan_merge(&sources, opts, host)?;
    write_merge(plan, output, opts, overrides, host)
}

/// What pass 1 learned, and everything pass 2 needs from it. The inputs
/// travel with the plan, so an entry's `source` can only ever be resolved
/// against the list it was indexed from.
struct MergePlan {
    /// One pin per argument, in argument order (a repeated argument shares
    /// its pin), consumed by pass 2's [`blob_merge::open_sources`].
    inputs: Vec<PinnedInput>,
    headers: Vec<crate::HeaderBlock>,
    plan: Plan,
}

/// Pass 1: check the inputs, index every blob, order the index and find the
/// overlap runs. The caller has admitted the descriptors.
fn plan_merge(
    inputs: &[InputSource<'_>],
    opts: &WriteOptions,
    host: &crate::host::Entered<'_>,
) -> Result<MergePlan> {
    let pins = crate::read::input::pin_sources(inputs, host)?;

    // Validate inputs have indexdata
    for input in &pins {
        crate::commands::require_indexdata(
            input,
            opts.direct_reads,
            opts.force,
            "input PBF has no blob-level indexdata. Without indexdata, every blob must be \
             decompressed to scan element IDs (significantly slower).",
            host,
        )?;
    }

    // MERGEPBF_START/END span both passes, from here (after the descriptor
    // admission, the pins and the indexdata probes) to the writer's flush
    // (before publication). Inside it MERGEPBF_PASS1 spans the index walk
    // and the plan, MERGEPBF_PASS2 the rest, with MERGEPBF_WRITE_LOOP (the
    // ordered write and the overlap sweeps beside it, the pool's join
    // included) and MERGEPBF_FLUSH (the writer draining) nested in it, as
    // sort's SORT_* phases are.
    host.invocation.marker("MERGEPBF_START");
    host.invocation.marker("MERGEPBF_PASS1_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "cat.dedupe.pass1",
        "Pass 1: indexing blobs..."
    );
    // A partially indexed input is accepted, because cat is the remedy for
    // one: each blob without indexdata is scanned, so the plan rests on
    // observed ranges for it (reference/indexdata-trust.md, "Missing and
    // partial indexdata").
    let (headers, indexed) = blob_merge::index_inputs(
        &pins,
        &IndexPolicy {
            require_sorted: Some(
                "Cat dedupe requires sorted inputs so it can merge them in type-then-ID order.",
            ),
            accept_partial_index: true,
            distrust_metadata: false,
        },
        opts.direct_reads,
        host,
    )?;
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "cat.dedupe.indexed",
        "  {} OSMData blobs indexed from {} files",
        indexed.frames.len() as u64 + indexed.omitted,
        inputs.len()
    );

    let plan = blob_merge::plan(indexed);
    if plan.blobs_rewritten > 0 {
        crate::host::diag!(
            host.diagnostics,
            Progress,
            "cat.dedupe.overlap_runs",
            "  {} blobs in overlap runs (decode + merge + dedup)",
            plan.blobs_rewritten
        );
    }
    if plan.restructured > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "cat.dedupe.restructured",
            "{} blobs held blocks no single index describes (kinds changing across \
             groups, or a group carrying more than one element field); each kind's \
             elements were decoded and re-encoded in type order, as overlapping blobs are",
            plan.restructured
        );
    }
    if plan.omitted > 0 {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "cat.dedupe.omitted",
            "{} blobs held no elements and were omitted, with any unknown fields they \
             carried",
            plan.omitted
        );
    }
    host.invocation
        .counter("cat_dedupe_blobs_total", plan.blobs_total());
    host.invocation
        .counter("cat_dedupe_blobs_omitted", plan.omitted);
    host.invocation
        .counter("cat_dedupe_overlap_runs", plan.runs.len());
    host.invocation.marker("MERGEPBF_PASS1_END");

    Ok(MergePlan {
        inputs: pins,
        headers,
        plan,
    })
}

/// Pass 2: write the planned blobs in order, re-reading each frame from the
/// input pass 1 found it in. A frame that no longer reads or decodes (the
/// file changed between passes) fails located at that input and the frame's
/// offset.
fn write_merge(
    merge: MergePlan,
    output: &crate::output::BoundOutput,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<MergePbfStats> {
    let MergePlan {
        inputs,
        headers,
        plan,
    } = merge;

    host.invocation.marker("MERGEPBF_PASS2_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "cat.dedupe.pass2",
        "Pass 2: writing merged output..."
    );
    // Reconciled across every input, not taken from the first: the merge's
    // header has to describe the merge. Always sorted - every input passed
    // the sortedness check and the sweep emits in type-then-ID order.
    let header = crate::commands::output_header::build(
        &headers,
        &crate::commands::output_header::OutputFacts::sorted(plan.ways()),
        overrides,
        host,
    )?;
    let mut writer =
        writer_from_header_bytes(output, opts.compression, &header.bytes, opts.backend, host)?;

    let sources = blob_merge::open_sources(
        inputs,
        &plan,
        opts.direct_reads,
        opts.backend.supports_copy_range(),
    )?;
    host.invocation.marker("MERGEPBF_WRITE_LOOP_START");
    let counts =
        blob_merge::write_plan(&plan, &sources, &mut writer, true, opts.compression, host)?;
    host.invocation.marker("MERGEPBF_WRITE_LOOP_END");
    // Input blobs, not output blocks: a rewritten blob is counted once
    // however many runs its kinds joined, and a run emits its own blocks.
    host.invocation
        .counter("cat_dedupe_blobs_passthrough", counts.blobs_passthrough);
    host.invocation
        .counter("cat_dedupe_blobs_rewritten", counts.blobs_rewritten);
    host.invocation
        .counter("cat_dedupe_copy_range_calls", counts.copy_range_calls);
    host.invocation.counter(
        "cat_dedupe_copy_range_coalesced",
        counts.copy_range_coalesced,
    );
    host.invocation
        .counter("cat_dedupe_duplicates_removed", counts.duplicates_removed);
    host.invocation.marker("MERGEPBF_FLUSH_START");
    writer.flush()?;
    host.invocation.marker("MERGEPBF_FLUSH_END");
    host.invocation.marker("MERGEPBF_PASS2_END");
    host.invocation.marker("MERGEPBF_END");
    Ok(MergePbfStats {
        nodes: counts.nodes,
        ways: counts.ways,
        relations: counts.relations,
        passthrough_claimed: counts.passthrough_claimed,
        blobs_passthrough: counts.blobs_passthrough,
        blobs_rewritten: counts.blobs_rewritten,
        blobs_omitted: plan.omitted,
        blobs_total: plan.blobs_total(),
        duplicates_removed: counts.duplicates_removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ElementKind;
    use crate::block_builder::BlockBuilder;
    use crate::commands::blob_merge::Frame;
    use crate::writer::PbfWriter;
    use std::path::Path;

    /// Kind of the one data blob a fixture writes into its own block.
    enum Blob<'a> {
        Nodes(&'a [i64]),
        Way(i64),
        Relation(i64),
    }

    /// A sorted, indexed PBF holding `blobs`, one block each, in order.
    fn write_fixture(path: &Path, blobs: &[Blob<'_>]) {
        write_fixture_indexed(path, blobs, true);
    }

    /// [`write_fixture`], the blobs carrying indexdata only when `indexed`.
    fn write_fixture_indexed(path: &Path, blobs: &[Blob<'_>], indexed: bool) {
        use crate::block_builder::HeaderBuilder;
        use crate::writer::Compression;
        let file = std::fs::File::create(path).expect("create fixture");
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::default(),
        );
        writer
            .write_header(&HeaderBuilder::new().sorted().build().expect("header"))
            .expect("write header");
        let none = std::iter::empty::<(&str, &str)>;
        let mut bb = BlockBuilder::new();
        for blob in blobs {
            match blob {
                Blob::Nodes(ids) => {
                    for &id in *ids {
                        bb.add_node(id, 0, 0, none(), None);
                    }
                }
                Blob::Way(id) => bb.add_way(*id, none(), &[1, 2], None),
                Blob::Relation(id) => bb.add_relation(*id, none(), &[], None),
            }
            let bytes = bb.take().expect("encode").expect("non-empty block");
            if indexed {
                writer.write_primitive_block(bytes).expect("write block");
            } else {
                writer
                    .write_primitive_block_no_indexdata(bytes)
                    .expect("write block");
            }
        }
        writer.flush().expect("flush fixture");
    }

    /// Input A: nodes 1..=3 and a way, input B: nodes 2..=4 (overlapping
    /// A's node blob) and a relation that overlaps nothing.
    fn fixtures(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let a = dir.join("a.osm.pbf");
        let b = dir.join("b.osm.pbf");
        write_fixture(&a, &[Blob::Nodes(&[1, 2, 3]), Blob::Way(10)]);
        write_fixture(&b, &[Blob::Nodes(&[2, 3, 4]), Blob::Relation(100)]);
        (a, b)
    }

    fn plan(a: &Path, b: &Path) -> MergePlan {
        plan_merge(
            &[a.into(), b.into()],
            &WriteOptions::default(),
            &crate::host::Host::entered_for_tests(),
        )
        .expect("pass 1")
    }

    fn run_pass2(dir: &Path, plan: MergePlan) -> Result<MergePbfStats> {
        let output = crate::output::Output::new(
            dir.join("merged.osm.pbf"),
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )?;
        let host = crate::host::Host::entered_for_tests();
        let output = output.bind(&host.invocation)?;
        write_merge(
            plan,
            &output,
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            &host,
        )
    }

    /// The frame of the entry of `kind` that pass 1 indexed from input
    /// `source`, and whether that entry is in an overlap run.
    fn entry_of(merge: &MergePlan, kind: ElementKind, source: usize) -> (&Frame, bool) {
        let frames = &merge.plan.frames;
        let (entry, &rewrite) = merge
            .plan
            .entries
            .iter()
            .zip(&merge.plan.rewrite)
            .find(|(e, _)| e.index.kind == kind && frames[e.frame].source == source)
            .expect("planned entry");
        (&frames[entry.frame], rewrite)
    }

    /// `--direct-io` reaches both passes: every input is walked `O_DIRECT`
    /// (the count is of direct walks, which the indexdata probe never
    /// opens), an unindexed input's payloads read through it, pass 2 reads
    /// its frames direct on an output that cannot splice, and the plan and
    /// the merge are the buffered run's.
    #[cfg(feature = "linux-direct-io")]
    #[test]
    fn direct_reads_walk_every_input_direct_in_pass_1() {
        use crate::read::header_walker::tests::direct_opens;
        let dir = crate::path_guard::tests::scratch();
        if !crate::test_support::direct_reads_supported(dir.path(), "the whole test") {
            return;
        }
        let (a, b) = fixtures(dir.path());
        let c = dir.path().join("c.osm.pbf");
        write_fixture_indexed(&c, &[Blob::Nodes(&[3, 4, 5]), Blob::Way(11)], false);
        let host = crate::host::Host::entered_for_tests();
        let opts = |direct_reads| WriteOptions {
            direct_reads,
            force: true,
            ..WriteOptions::default()
        };
        let summary = |merge: &MergePlan| {
            let frames = &merge.plan.frames;
            merge
                .plan
                .entries
                .iter()
                .zip(&merge.plan.rewrite)
                .map(|(e, &rewrite)| {
                    let f = &frames[e.frame];
                    (
                        (f.source, f.file_offset, f.frame_len, f.reindex.is_some()),
                        (e.index.kind, e.index.min_id, e.index.max_id, e.index.count),
                        (e.index_scanned, e.intra_unsorted, e.restructured, rewrite),
                    )
                })
                .collect::<Vec<_>>()
        };
        let before = direct_opens();
        let sources: [InputSource<'_>; 3] = [(&a).into(), (&b).into(), (&c).into()];
        let buffered = plan_merge(&sources, &opts(false), &host).expect("buffered pass 1");
        assert_eq!(direct_opens(), before, "a buffered run walked direct");
        let direct = plan_merge(&sources, &opts(true), &host).expect("direct pass 1");
        assert_eq!(
            direct_opens(),
            before + 3,
            "pass 1 did not walk every input direct"
        );
        assert_eq!(summary(&direct), summary(&buffered));
        assert!(
            summary(&direct)
                .iter()
                .any(|(_, _, (scanned, ..))| *scanned),
            "the unindexed input's payloads were scanned"
        );

        let merged = |merge, opts: &WriteOptions, name: &str| {
            let path = dir.path().join(name);
            let output = crate::output::Output::new(
                path.clone(),
                crate::output::Overwrite::Refuse,
                &crate::host::Diagnostics::discard(),
            )
            .expect("output");
            let output = output.bind(&host.invocation).expect("open");
            let stats = write_merge(merge, &output, opts, &HeaderOverrides::default(), &host)
                .expect("pass 2");
            output.publish().expect("publish");
            (
                (stats.nodes, stats.ways, stats.relations),
                (stats.blobs_rewritten, stats.duplicates_removed),
                std::fs::read(&path).expect("read merged"),
            )
        };
        let direct_pass2 = WriteOptions {
            backend: crate::write::OutputBackend::Direct,
            ..opts(true)
        };
        assert_eq!(
            merged(direct, &direct_pass2, "direct.osm.pbf"),
            merged(buffered, &WriteOptions::default(), "buffered.osm.pbf")
        );
    }

    /// The untouched plan merges: the two shared nodes are deduped.
    #[test]
    fn pass2_merges_an_unchanged_plan() {
        let dir = crate::path_guard::tests::scratch();
        let (a, b) = fixtures(dir.path());
        let stats = run_pass2(dir.path(), plan(&a, &b)).expect("pass 2");
        assert_eq!(stats.nodes, 4);
        assert_eq!(stats.duplicates_removed, 2);
        assert_eq!(stats.blobs_rewritten, 2);
        assert_eq!(stats.blobs_passthrough, 2);
    }

    // The two failure cases drive pass 2's frame reads (`read_block` for an
    // overlap run, `FrameBuf::read` for a passthrough blob) over a real pass-1
    // plan rather than `write_merge` itself: an error return drops pass 2's
    // `PbfWriter` unflushed, which this crate's unit-test builds assert
    // against (see the writer's `Drop`).

    /// A blob of an overlap run that no longer decodes when pass 2 re-reads
    /// it fails located at its input and its frame's offset - the second
    /// input, at a nonzero offset, so first-input attribution cannot pass.
    #[test]
    fn pass2_decode_failure_names_the_input_and_frame() {
        let dir = crate::path_guard::tests::scratch();
        let (a, b) = fixtures(dir.path());
        let merge = plan(&a, &b);
        let (entry, rewrite) = entry_of(&merge, ElementKind::Node, 1);
        assert!(rewrite, "B's node blob overlaps A's");
        assert!(entry.file_offset > 0, "the header blob precedes it");

        // Keep the length and the BlobHeader; make the Blob unparseable.
        let mut bytes = std::fs::read(&b).expect("read B");
        let start = usize::try_from(entry.file_offset).expect("offset");
        let header_len = u32::from_be_bytes(bytes[start..start + 4].try_into().expect("prefix"));
        let payload = start + 4 + header_len as usize;
        let end = start + usize::try_from(entry.frame_len).expect("frame len");
        bytes[payload..end].fill(0xff);
        std::fs::write(&b, &bytes).expect("corrupt B");

        for direct in pass2_transports(dir.path()) {
            let sources =
                blob_merge::open_sources(merge.inputs.clone(), &merge.plan, direct, false)
                    .expect("open inputs");
            assert_eq!(sources.direct(), direct);
            let mut buf = blob_merge::FrameBuf::default();
            let err = match blob_merge::read_block(&sources[entry.source], entry, &mut buf) {
                Ok(_) => panic!("a corrupt blob must fail pass 2"),
                Err(err) => err,
            };
            assert!(
                matches!(err.kind(), crate::ErrorKind::WireFormat { .. }),
                "direct {direct}: expected the Blob decode failure, got {err:?}"
            );
            assert_eq!(err.source_path(), Some(b.as_path()));
            assert_eq!(
                err.byte_offset(),
                Some(crate::ByteOffset(entry.file_offset))
            );
        }
    }

    /// The pass-2 transports this host can run: buffered, plus direct where
    /// the build and the scratch filesystem allow it.
    fn pass2_transports(dir: &std::path::Path) -> Vec<bool> {
        #[cfg_attr(not(feature = "linux-direct-io"), allow(unused_mut))]
        let mut transports = vec![false];
        #[cfg(feature = "linux-direct-io")]
        if crate::test_support::direct_reads_supported(dir, "the direct pass-2 read") {
            transports.push(true);
        }
        let _ = dir;
        transports
    }

    /// A passthrough frame that is gone when pass 2 re-reads it (the input
    /// was truncated between passes) fails located at its input and offset.
    #[test]
    fn pass2_read_failure_names_the_input_and_frame() {
        let dir = crate::path_guard::tests::scratch();
        let (a, b) = fixtures(dir.path());
        let merge = plan(&a, &b);
        let (entry, rewrite) = entry_of(&merge, ElementKind::Relation, 1);
        assert!(!rewrite, "B's relation blob passes through");

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&b)
            .expect("open B");
        file.set_len(entry.file_offset)
            .expect("truncate B at the relation frame");
        drop(file);

        for direct in pass2_transports(dir.path()) {
            let sources =
                blob_merge::open_sources(merge.inputs.clone(), &merge.plan, direct, false)
                    .expect("open inputs");
            assert_eq!(sources.direct(), direct);
            // This plan copies a frame through, so an output that splices
            // keeps the buffered descriptor the copy needs.
            let splicing =
                blob_merge::open_sources(merge.inputs.clone(), &merge.plan, direct, true)
                    .expect("open inputs");
            assert!(!splicing.direct(), "a direct source would meet a splice");
            let mut buf = blob_merge::FrameBuf::default();
            let err = match buf.read(&sources[entry.source], entry) {
                Ok(_) => panic!("a missing frame must fail pass 2"),
                Err(err) => err,
            };
            assert!(
                matches!(err.kind(), crate::ErrorKind::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof),
                "direct {direct}: expected the frame read to hit end of file, got {err:?}"
            );
            assert_eq!(err.source_path(), Some(b.as_path()));
            assert_eq!(
                err.byte_offset(),
                Some(crate::ByteOffset(entry.file_offset))
            );
        }
    }
}
