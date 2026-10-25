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

use std::path::Path;

use crate::commands::blob_merge::{self, IndexPolicy, Plan};
use crate::commands::{HeaderOverrides, writer_from_header_bytes};
use crate::error::{Error, Result};
use crate::read::input::PinnedInput;
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
    pub blobs_total: u64,
    pub duplicates_removed: u64,
}

impl MergePbfStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let total = self.nodes + self.ways + self.relations;
        writeln!(
            w,
            "Merged {total} elements: {} nodes, {} ways, {} relations \
             ({} blobs: {} passthrough, {} rewritten, {} duplicates removed)",
            self.nodes,
            self.ways,
            self.relations,
            self.blobs_total,
            self.blobs_passthrough,
            self.blobs_rewritten,
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
/// each is read from the file the run started on. Pass 2 holds one source
/// per argument, so the run asks the host for one descriptor per argument
/// (`ProcessActions::fd_allowance`) and is refused up front when allowed
/// fewer.
pub fn merge_pbf(
    inputs: &[&Path],
    output: crate::output::Output,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<MergePbfStats> {
    host.cancel.check()?;
    let result = merge_pbf_to(inputs, &output, opts, overrides, host)?;
    output.publish(&host.cancel)?;
    Ok(result)
}

/// [`merge_pbf`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn merge_pbf_to(
    inputs: &[&Path],
    output: &crate::output::Output,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<MergePbfStats> {
    let plan = plan_merge(inputs, opts, host)?;
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
/// overlap runs.
fn plan_merge(
    inputs: &[&Path],
    opts: &WriteOptions,
    host: &crate::host::Host,
) -> Result<MergePlan> {
    if inputs.is_empty() {
        return Err(Error::invalid_argument("no input files specified"));
    }

    // Every input is opened three times (the indexdata probe, pass 1's
    // index walk, pass 2's source), so every input is pinned up front. The
    // pins are held until pass 2 has opened its sources, one per argument
    // and held for the whole of pass 2, so the run asks for one descriptor
    // per argument.
    super::admit_descriptors(
        inputs.len(),
        inputs.len(),
        crate::read::input::distinct_inputs(inputs),
        host,
    )?;
    let pins = crate::read::input::pin_inputs(inputs, host)?;

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

    crate::debug::emit_marker("MERGEPBF_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "cat.dedupe.pass1",
        "Pass 1: indexing blobs..."
    );
    // A partially indexed input is accepted: each blob without indexdata is
    // scanned, so the plan rests on observed ranges for it
    // (reference/indexdata-trust.md, "Missing and partial indexdata").
    let (headers, entries) = blob_merge::index_inputs(
        &pins,
        &IndexPolicy {
            require_sorted: Some(
                "Cat dedupe requires sorted inputs so it can merge them in type-then-ID order.",
            ),
            refuse_partial_index: false,
        },
        host,
    )?;
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "cat.dedupe.indexed",
        "  {} OSMData blobs indexed from {} files",
        entries.len(),
        inputs.len()
    );

    let plan = blob_merge::plan(entries);
    let rewritten = plan.overlapping + plan.intra_unsorted;
    if rewritten > 0 {
        crate::host::diag!(
            host.diagnostics,
            Progress,
            "cat.dedupe.overlap_runs",
            "  {rewritten} blobs in overlap runs (decode + merge + dedup)"
        );
    }

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
    output: &crate::output::Output,
    opts: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<MergePbfStats> {
    let MergePlan {
        inputs,
        headers,
        plan,
    } = merge;

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

    let sources = blob_merge::open_sources(inputs)?;
    let counts = blob_merge::write_plan(
        &plan,
        &sources,
        &mut writer,
        true,
        opts.backend.supports_copy_range(),
        host,
    )?;
    writer.flush()?;
    crate::debug::emit_marker("MERGEPBF_END");
    Ok(MergePbfStats {
        nodes: counts.nodes,
        ways: counts.ways,
        relations: counts.relations,
        passthrough_claimed: counts.passthrough_claimed,
        blobs_passthrough: counts.blobs_passthrough,
        blobs_rewritten: counts.blobs_rewritten,
        blobs_total: plan.entries.len() as u64,
        duplicates_removed: counts.duplicates_removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ElementKind;
    use crate::block_builder::BlockBuilder;
    use crate::commands::blob_merge::BlobEntry;
    use crate::writer::PbfWriter;

    /// Kind of the one data blob a fixture writes into its own block.
    enum Blob<'a> {
        Nodes(&'a [i64]),
        Way(i64),
        Relation(i64),
    }

    /// A sorted, indexed PBF holding `blobs`, one block each, in order.
    fn write_fixture(path: &Path, blobs: &[Blob<'_>]) {
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
            writer.write_primitive_block(bytes).expect("write block");
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
            &[a, b],
            &WriteOptions::default(),
            &crate::host::Host::for_tests(),
        )
        .expect("pass 1")
    }

    fn run_pass2(dir: &Path, plan: MergePlan) -> Result<MergePbfStats> {
        let output = crate::output::Output::new(
            dir.join("merged.osm.pbf"),
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )?;
        write_merge(
            plan,
            &output,
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            &crate::host::Host::for_tests(),
        )
    }

    /// The entry of `kind` that pass 1 indexed from input `source`, and
    /// whether it is in an overlap run.
    fn entry_of(merge: &MergePlan, kind: ElementKind, source: usize) -> (&BlobEntry, bool) {
        let (entry, &rewrite) = merge
            .plan
            .entries
            .iter()
            .zip(&merge.plan.rewrite)
            .find(|(e, _)| e.index.kind == kind && e.source == source)
            .expect("planned entry");
        (entry, rewrite)
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
    // overlap run, `read_frame` for a passthrough blob) over a real pass-1
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

        let sources = blob_merge::open_sources(merge.inputs.clone()).expect("open inputs");
        let err = match blob_merge::read_block(&sources[entry.source], entry, &mut Vec::new()) {
            Ok(_) => panic!("a corrupt blob must fail pass 2"),
            Err(err) => err,
        };
        assert!(
            matches!(err.kind(), crate::ErrorKind::WireFormat { .. }),
            "expected the Blob decode failure, got {err:?}"
        );
        assert_eq!(err.source_path(), Some(b.as_path()));
        assert_eq!(
            err.byte_offset(),
            Some(crate::ByteOffset(entry.file_offset))
        );
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

        let sources = blob_merge::open_sources(merge.inputs.clone()).expect("open inputs");
        let err = match blob_merge::read_frame(&sources[entry.source], entry, &mut Vec::new()) {
            Ok(()) => panic!("a missing frame must fail pass 2"),
            Err(err) => err,
        };
        assert!(
            matches!(err.kind(), crate::ErrorKind::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof),
            "expected the frame read to hit end of file, got {err:?}"
        );
        assert_eq!(err.source_path(), Some(b.as_path()));
        assert_eq!(
            err.byte_offset(),
            Some(crate::ByteOffset(entry.file_offset))
        );
    }
}
