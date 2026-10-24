//! Parallel shard-based block-pair merge for `diff-snapshots`.
//!
//! Splits the ID space into N shards and runs an independent block-pair
//! merge per shard on a worker thread. Each shard is self-contained:
//! it owns its slice of old-side blobs and new-side blobs, runs its
//! merge loop with pread from a shared File, and streams output to a
//! per-shard scratch temp file. Main thread concatenates those temp
//! files to the caller's output in shard order. A count-only run
//! (`diff_stats`) shards and merges the same way but formats nothing, so
//! it writes no scratch and its shards hand back counts alone.
//!
//! The temp-file output shape keeps steady-state anon RSS flat; an
//! earlier in-memory `Vec<u8>` shape buffered the full per-shard text
//! until join, peaking at ~2.3 GB on a planet diff at `-j 16`. The
//! sibling `--format osc` driver in `derive_parallel.rs` already used
//! per-shard temp files for the same reason.

use std::io::{self, BufWriter, Write};
use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::sync::Arc;

use crate::ElementKind;
use crate::PrimitiveBlock;
use crate::blob_meta::BlobIndex;
use crate::error::Result;
use crate::owned::TypeFilter;

use super::shard::{BlobDesc, Shard, plan_shards, walk_pair};
use super::{DiffOptions, DiffStats};

// ---------------------------------------------------------------------------
// Per-shard worker: in-memory block-pair merge
// ---------------------------------------------------------------------------

/// Where a text run's shard output goes: each shard formats into a file in
/// `scratch`, and the driver streams the files to `out` in shard order. A
/// count-only run has none of this; its shards format nothing and create no
/// file.
pub(super) struct ShardText<'a, W> {
    pub(super) out: &'a mut W,
    pub(super) scratch: &'a crate::scratch::RunScratch,
}

/// What one shard worker produces.
struct ShardOutput {
    /// Path to the per-shard scratch temp file holding this shard's
    /// formatted diff text, `None` for a count-only run. The driver streams
    /// it in shard order to the caller's output and then removes it.
    text_path: Option<crate::scratch::ScratchPath>,
    /// Per-shard stats, aggregated into the caller's `DiffStats`.
    stats: DiffStats,
}

/// Pread a blob's compressed data and decompress + parse into a `PrimitiveBlock`.
fn pread_decode(
    file: &std::fs::File,
    source: &Arc<Path>,
    desc: BlobDesc,
    read_buf: &mut Vec<u8>,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
) -> Result<PrimitiveBlock> {
    let locate = |e: crate::error::Error| {
        e.locate(crate::error::Location::new(
            Some(source),
            Some(desc.blob_index),
            Some(crate::ByteOffset(desc.frame_offset)),
        ))
    };
    read_buf.resize(desc.data_size, 0);
    file.read_exact_at(read_buf, desc.data_offset)
        .map_err(|e| locate(e.into()))?;
    let mut decompressed: Vec<u8> = Vec::new();
    crate::blob::decompress_blob_raw(read_buf, &mut decompressed).map_err(locate)?;
    PrimitiveBlock::from_vec_with_scratch(decompressed, st_scratch, gr_scratch).map_err(locate)
}

/// Decoded-side state carried between iterations within a shard.
struct Side {
    block: PrimitiveBlock,
    skip_count: usize,
    index: BlobIndex,
}

/// Run one shard's block-pair merge. With `scratch`, streams formatted text
/// to a per-shard scratch temp file and returns its path + stats; without
/// it (a count-only run), formats nothing, creates no file and returns the
/// stats alone, having merged exactly the same elements.
///
/// This is a self-contained reimplementation of the `block_pair_merge_phase`
/// control flow, scoped to the shard's blob index ranges. It assumes both
/// inputs are sorted and that the blob lists carry indexdata (established
/// by the walker).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_shard(
    shard: Shard,
    shard_idx: usize,
    old_descs: &[BlobDesc],
    new_descs: &[BlobDesc],
    old_file: &std::fs::File,
    new_file: &std::fs::File,
    old_source: &Arc<Path>,
    new_source: &Arc<Path>,
    kind: ElementKind,
    options: &DiffOptions,
    scratch: Option<&crate::scratch::RunScratch>,
    cancel: &crate::host::Cancel,
) -> Result<ShardOutput> {
    // Test-only: the fault plan's `diff_shard_panic`. Simulates a
    // mid-phase shard crash. The caller's `scope.spawn(...).join()`
    // path turns the panic into an `Err("shard worker panicked: ..")` and
    // the surrounding cleanup pass sweeps every possible per-shard
    // scratch file. Release builds compile this out entirely.
    #[cfg(feature = "test-hooks")]
    if cancel
        .faults()
        .diff_shard_panic
        .load(std::sync::atomic::Ordering::Relaxed)
        == shard_idx
    {
        panic!("test-hooks: diff/parallel shard {shard_idx} panicking");
    }

    let type_char = kind.type_char();
    let (text_path, mut out) = match scratch {
        Some(scratch) => {
            let text_path = shard_text_path(scratch, kind, shard_idx)?;
            let file = crate::scratch::create_file(&text_path)?;
            (Some(text_path), Some(BufWriter::new(file)))
        }
        None => (None, None),
    };

    let mut stats = DiffStats::default();

    let old_slice = &old_descs[shard.old_start..shard.old_end];
    let new_slice = &new_descs[shard.new_start..shard.new_end];
    let t_low = shard.t_low;
    let t_high = shard.t_high;

    let mut old_idx = 0usize;
    let mut new_idx = 0usize;
    let mut old_decoded: Option<Side> = None;
    let mut new_decoded: Option<Side> = None;

    // Per-worker scratch. Kept across iterations to amortize allocation.
    let mut old_read: Vec<u8> = Vec::new();
    let mut new_read: Vec<u8> = Vec::new();
    let mut old_st: Vec<(u32, u32)> = Vec::new();
    let mut old_gr: Vec<(u32, u32)> = Vec::new();
    let mut new_st: Vec<(u32, u32)> = Vec::new();
    let mut new_gr: Vec<(u32, u32)> = Vec::new();

    loop {
        // Once per merge step, each of which decodes at most one blob per
        // side. Shards are independent threads joined by the driver, so a
        // cancelled shard simply returns; there is no sequence to keep.
        cancel.check()?;
        // Ensure both sides have a decoded block (or we've exhausted one side).
        if old_decoded.is_none() && old_idx < old_slice.len() {
            let desc = old_slice[old_idx];
            old_idx += 1;
            let block = pread_decode(
                old_file,
                old_source,
                desc,
                &mut old_read,
                &mut old_st,
                &mut old_gr,
            )?;
            old_decoded = Some(Side {
                block,
                skip_count: 0,
                index: desc.index,
            });
        }
        if new_decoded.is_none() && new_idx < new_slice.len() {
            let desc = new_slice[new_idx];
            new_idx += 1;
            let block = pread_decode(
                new_file,
                new_source,
                desc,
                &mut new_read,
                &mut new_st,
                &mut new_gr,
            )?;
            new_decoded = Some(Side {
                block,
                skip_count: 0,
                index: desc.index,
            });
        }

        match (&old_decoded, &new_decoded) {
            (None, None) => break,
            (Some(_), None) => {
                let os = old_decoded.take().expect("checked");
                emit_side(&mut out, &os, true, type_char, t_low, t_high, &mut stats)?;
            }
            (None, Some(_)) => {
                let ns = new_decoded.take().expect("checked");
                emit_side(&mut out, &ns, false, type_char, t_low, t_high, &mut stats)?;
            }
            (Some(os), Some(ns)) => {
                if os.index.max_id < ns.index.min_id {
                    let os = old_decoded.take().expect("checked");
                    emit_side(&mut out, &os, true, type_char, t_low, t_high, &mut stats)?;
                    continue;
                }
                if ns.index.max_id < os.index.min_id {
                    let ns = new_decoded.take().expect("checked");
                    emit_side(&mut out, &ns, false, type_char, t_low, t_high, &mut stats)?;
                    continue;
                }
                // Overlapping - merge element by element.
                merge_decoded(
                    &mut old_decoded,
                    &mut new_decoded,
                    type_char,
                    t_low,
                    t_high,
                    options,
                    &mut out,
                    &mut stats,
                )?;
            }
        }
    }

    // BufWriter::into_inner propagates any deferred flush error; dropping
    // the unwrapped File closes it and the main thread reopens the path.
    if let Some(out) = out {
        let _ = out
            .into_inner()
            .map_err(|e| crate::error::Error::from(e.into_error()))?;
    }
    Ok(ShardOutput { text_path, stats })
}

/// Emit elements of a decoded block (single-sided: all OldOnly or all
/// NewOnly) clipped to the shard's ID window `(t_low, t_high]`. A
/// count-only run (`out` is `None`) still walks the IDs: the clip is what
/// decides which elements this shard counts.
#[allow(clippy::too_many_arguments)]
fn emit_side(
    out: &mut Option<impl Write>,
    side: &Side,
    is_old: bool,
    type_char: char,
    t_low: i64,
    t_high: i64,
    stats: &mut DiffStats,
) -> Result<()> {
    let prefix = if is_old { '-' } else { '+' };
    for elem in side.block.elements().skip(side.skip_count) {
        let id = crate::osc::merge_join::element_id(&elem);
        if id <= t_low {
            continue;
        }
        if id > t_high {
            break;
        }
        if let Some(out) = out.as_mut() {
            let version = crate::osc::merge_join::element_version(&elem);
            emit_element(out, prefix, type_char, id, version)?;
        }
        if is_old {
            stats.deleted += 1;
        } else {
            stats.created += 1;
        }
    }
    Ok(())
}

/// Merge two overlapping decoded blocks, updating residuals in place.
#[allow(clippy::too_many_arguments)]
fn merge_decoded(
    old_decoded: &mut Option<Side>,
    new_decoded: &mut Option<Side>,
    type_char: char,
    t_low: i64,
    t_high: i64,
    options: &DiffOptions,
    out: &mut Option<impl Write>,
    stats: &mut DiffStats,
) -> Result<()> {
    let mut os = old_decoded.take().expect("checked");
    let mut ns = new_decoded.take().expect("checked");

    let merge_up_to = os.index.max_id.min(ns.index.max_id).min(t_high);
    let (old_consumed, new_consumed) = element_merge(
        &os.block,
        os.skip_count,
        &ns.block,
        ns.skip_count,
        merge_up_to,
        t_low,
        type_char,
        options,
        out,
        stats,
    )?;

    if os.index.max_id > merge_up_to {
        os.skip_count += old_consumed;
        *old_decoded = Some(os);
    }
    if ns.index.max_id > merge_up_to {
        ns.skip_count += new_consumed;
        *new_decoded = Some(ns);
    }
    Ok(())
}

/// Two-pointer element merge over a pair of decoded blocks. Processes
/// elements up to `merge_up_to` ID (inclusive). Returns consumed counts
/// per side. `out` is `None` for a count-only run: the merge, the content
/// comparison and the counts are the same, only nothing is formatted.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn element_merge(
    old_block: &PrimitiveBlock,
    old_skip: usize,
    new_block: &PrimitiveBlock,
    new_skip: usize,
    merge_up_to: i64,
    t_low: i64,
    type_char: char,
    options: &DiffOptions,
    out: &mut Option<impl Write>,
    stats: &mut DiffStats,
) -> Result<(usize, usize)> {
    use crate::osc::merge_join::{element_id, element_version};

    let mut old_iter = old_block.elements().skip(old_skip).peekable();
    let mut new_iter = new_block.elements().skip(new_skip).peekable();
    let mut old_consumed = 0usize;
    let mut new_consumed = 0usize;

    // Skip elements whose ID is at or below t_low - those belong to
    // the previous shard. Track the consumed count so the caller can
    // update its skip_count if this blob remains as a residual.
    while old_iter.peek().is_some_and(|e| element_id(e) <= t_low) {
        old_iter.next();
        old_consumed += 1;
    }
    while new_iter.peek().is_some_and(|e| element_id(e) <= t_low) {
        new_iter.next();
        new_consumed += 1;
    }

    loop {
        let old_in_range = old_iter
            .peek()
            .is_some_and(|e| element_id(e) <= merge_up_to);
        let new_in_range = new_iter
            .peek()
            .is_some_and(|e| element_id(e) <= merge_up_to);

        match (old_in_range, new_in_range) {
            (false, false) => break,
            (true, false) => {
                let o = old_iter.next().expect("checked peek");
                if let Some(w) = out.as_mut() {
                    emit_element(w, '-', type_char, element_id(&o), element_version(&o))?;
                }
                stats.deleted += 1;
                old_consumed += 1;
            }
            (false, true) => {
                let n = new_iter.next().expect("checked peek");
                if let Some(w) = out.as_mut() {
                    emit_element(w, '+', type_char, element_id(&n), element_version(&n))?;
                }
                stats.created += 1;
                new_consumed += 1;
            }
            (true, true) => {
                let o_id = element_id(old_iter.peek().expect("checked"));
                let n_id = element_id(new_iter.peek().expect("checked"));
                match crate::osm_id::osm_id_cmp(o_id, n_id) {
                    std::cmp::Ordering::Less => {
                        let o = old_iter.next().expect("checked");
                        if let Some(w) = out.as_mut() {
                            emit_element(w, '-', type_char, element_id(&o), element_version(&o))?;
                        }
                        stats.deleted += 1;
                        old_consumed += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        let n = new_iter.next().expect("checked");
                        if let Some(w) = out.as_mut() {
                            emit_element(w, '+', type_char, element_id(&n), element_version(&n))?;
                        }
                        stats.created += 1;
                        new_consumed += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        let o = old_iter.next().expect("checked");
                        let n = new_iter.next().expect("checked");
                        old_consumed += 1;
                        new_consumed += 1;
                        if crate::content::content_equal(&o, &n) {
                            if !options.suppress_common
                                && let Some(w) = out.as_mut()
                            {
                                emit_element(w, ' ', type_char, o_id, element_version(&o))?;
                            }
                            stats.common += 1;
                        } else {
                            if let Some(w) = out.as_mut() {
                                let old_ver = element_version(&o);
                                let new_ver = element_version(&n);
                                emit_modified(w, type_char, o_id, old_ver, new_ver)?;
                            }
                            // Verbose details are not supported on the parallel
                            // path. The dispatcher in mod.rs routes verbose
                            // diffs to the sequential `diff_block_pair`, so
                            // this arm never runs with verbose set.
                            debug_assert!(!options.verbose);
                            stats.modified += 1;
                        }
                    }
                }
            }
        }
    }

    Ok((old_consumed, new_consumed))
}

/// A shard's scratch text file.
fn shard_text_path(
    scratch: &crate::scratch::RunScratch,
    kind: ElementKind,
    shard_idx: usize,
) -> Result<crate::scratch::ScratchPath> {
    let kind_tag = kind.type_char();
    scratch.file(&format!("diff-par-{kind_tag}-{shard_idx}.txt.tmp"))
}

fn emit_element(
    out: &mut impl Write,
    prefix: char,
    type_char: char,
    id: i64,
    version: Option<i32>,
) -> Result<()> {
    match version {
        Some(v) => writeln!(out, "{prefix}{type_char}{id} v{v}"),
        None => writeln!(out, "{prefix}{type_char}{id}"),
    }
    .map_err(crate::error::Error::from)
}

fn emit_modified(
    out: &mut impl Write,
    type_char: char,
    id: i64,
    old_version: Option<i32>,
    new_version: Option<i32>,
) -> Result<()> {
    match (old_version, new_version) {
        (Some(ov), Some(nv)) if ov != nv => {
            writeln!(out, "*{type_char}{id} v{ov} -> v{nv}")
        }
        (_, Some(v)) => writeln!(out, "*{type_char}{id} v{v}"),
        (Some(v), None) => writeln!(out, "*{type_char}{id} v{v}"),
        (None, None) => writeln!(out, "*{type_char}{id}"),
    }
    .map_err(crate::error::Error::from)
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Parallel variant of `diff_block_pair`. Shards the ID space, runs each
/// shard on a worker thread, and concatenates outputs in order.
///
/// Preserves diff semantics for the `-c` (suppress-common) and verbose
/// cases; drops the v1 byte-equal fast path (always zero fires on
/// `diff-snapshots`, where this path is intended to be used).
///
/// `text` is `None` for a count-only run, which shards and merges exactly
/// as a text run but formats nothing and writes no scratch.
///
/// Returns `Ok(None)`, having written nothing, when either input holds a
/// negative ID: the caller must run the sequential block-pair merge instead
/// (see `shard`'s correctness contract).
#[allow(clippy::too_many_lines)] // phase-orchestrator over 3 element kinds
#[allow(clippy::too_many_arguments)]
pub(super) fn diff_block_pair_parallel<W: Write>(
    old_path: &Path,
    new_path: &Path,
    mut text: Option<ShardText<'_, W>>,
    options: &DiffOptions,
    _direct_io: bool,
    filter: &TypeFilter,
    shard_count: usize,
    host: &crate::host::Host,
) -> Result<Option<DiffStats>> {
    let scratch = text.as_ref().map(|t| t.scratch);
    // DIFF_SCAN_START/END and diff_common/created/modified/deleted counters
    // are emitted by the calling `diff()` function in `mod.rs`; do not
    // re-emit here.
    let Some((old, new)) = walk_pair(old_path, new_path, host)? else {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "diff.negative_id_fallback",
            "Using sequential comparison: negative IDs cannot use parallel ID-range shards"
        );
        crate::debug::emit_counter("diff_negative_id_fallback", 1);
        return Ok(None);
    };

    // Scratch temp files live in the run's scratch directory, which the caller
    // placed (the CLI's --tmp-dir, else beside the output).
    // No input-size-based free-space refusal: compressed PBF sizes and index
    // counts do not bound changed text bytes (identical inputs emit none).
    // Exact sizing would repeat the measured comparison/serialization pass.
    // Shards coexist until this kind is drained, then are removed one by one;
    // the caller's arbitrary Write destination may share this filesystem or
    // consume no disk at all, so twice the input size is not a space budget.
    // Keep write errors authoritative rather than rejecting on that guess.
    let mut total = DiffStats::default();

    struct PhaseSlot<'a> {
        kind: ElementKind,
        enabled: bool,
        tag: &'static str,
        old: &'a [BlobDesc],
        new: &'a [BlobDesc],
    }
    let phases = [
        PhaseSlot {
            kind: ElementKind::Node,
            enabled: filter.nodes,
            tag: "NODE",
            old: &old.nodes,
            new: &new.nodes,
        },
        PhaseSlot {
            kind: ElementKind::Way,
            enabled: filter.ways,
            tag: "WAY",
            old: &old.ways,
            new: &new.ways,
        },
        PhaseSlot {
            kind: ElementKind::Relation,
            enabled: filter.relations,
            tag: "REL",
            old: &old.relations,
            new: &new.relations,
        },
    ];

    for phase in phases {
        if !phase.enabled {
            continue;
        }
        if phase.old.is_empty() && phase.new.is_empty() {
            continue;
        }
        let kind = phase.kind;
        let old_descs = phase.old;
        let new_descs = phase.new;
        let tag = phase.tag;

        let start_marker = format!("DIFF_PHASE_{tag}_START");
        let end_marker = format!("DIFF_PHASE_{tag}_END");
        crate::debug::emit_marker(&start_marker);

        let shards = plan_shards(old_descs, new_descs, shard_count);

        let mut shard_outputs: Vec<Option<Result<ShardOutput>>> =
            (0..shards.len()).map(|_| None).collect();

        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(shards.len());
            for (shard_idx, shard) in shards.iter().copied().enumerate() {
                let old_file = Arc::clone(&old.file);
                let new_file = Arc::clone(&new.file);
                let old_source = &old.path;
                let new_source = &new.path;
                let h = s.spawn(move || {
                    run_shard(
                        shard,
                        shard_idx,
                        old_descs,
                        new_descs,
                        &old_file,
                        &new_file,
                        old_source,
                        new_source,
                        kind,
                        options,
                        scratch,
                        &host.cancel,
                    )
                });
                handles.push(h);
            }
            for (idx, h) in handles.into_iter().enumerate() {
                shard_outputs[idx] = Some(h.join().unwrap_or_else(|payload| {
                    Err(crate::error::panic_error("shard worker", payload.as_ref()))
                }));
            }
        });

        // Partition shard outputs into (successful, first error). On an
        // error every shard file is left for the run directory's guard.
        // Shards run independently, so slot order is not time order: a
        // real failure in any shard outranks the cancellation the others
        // observed, which a signal racing that failure would otherwise mask.
        let mut outputs: Vec<ShardOutput> = Vec::with_capacity(shard_outputs.len());
        let mut first_err: Option<crate::error::Error> = None;
        for slot in shard_outputs {
            match slot.expect("all slots filled") {
                Ok(out) => outputs.push(out),
                Err(e) => {
                    if first_err
                        .as_ref()
                        .is_none_or(|kept| kept.is_cancelled() && !e.is_cancelled())
                    {
                        first_err = Some(e);
                    }
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }

        // All successful: emit shard outputs in shard order, streaming each
        // per-shard temp file to the caller's output and removing it. A
        // count-only run has no files and folds the counts directly.
        match text.as_mut() {
            Some(text) => emit_shard_outputs(
                &mut *text.out,
                outputs,
                &mut total,
                text.scratch,
                &host.cancel,
            )?,
            None => {
                for shard_out in outputs {
                    total.add(&shard_out.stats);
                }
            }
        }

        crate::debug::emit_marker(&end_marker);
    }

    Ok(Some(total))
}

/// Stream every shard's temp file to `output` in shard order, folding each
/// shard's counts into `total`.
///
/// A failed write to `output` - the reader of a stdout pipe going away is
/// the ordinary case (`diff a b | head`) - stops the emission, but every
/// remaining shard's temp file is still removed before the error returns.
/// Returning on the first error left them all in the scratch directory
/// beside the old input, one per shard per run.
fn emit_shard_outputs(
    output: &mut impl Write,
    outputs: Vec<ShardOutput>,
    total: &mut DiffStats,
    scratch: &crate::scratch::RunScratch,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    let mut first_err = None;
    for shard_out in outputs {
        let text_path = shard_out
            .text_path
            .as_ref()
            .expect("every shard of a text run writes its text file");
        if first_err.is_some() {
            scratch.remove(text_path);
            continue;
        }
        if let Err(e) = append_and_cleanup(output, text_path, scratch, cancel) {
            first_err = Some(e);
            continue;
        }
        total.add(&shard_out.stats);
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    Ok(())
}

/// Copy a shard's scratch temp file to the caller's output writer and then
/// remove it - also when the copy fails, so a failed write does not leave
/// the temp file behind. Mirrors `derive_parallel::append_and_cleanup`.
fn append_and_cleanup(
    out: &mut impl Write,
    src: &crate::scratch::ScratchPath,
    scratch: &crate::scratch::RunScratch,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    // The copy checks the token at every read of the shard file (a shard
    // can hold gigabytes of text, and stdout is not cancellation-aware).
    let copied = crate::scratch::open(src)
        .and_then(|f| io::copy(&mut crate::host::CancellableRead::new(f, cancel), out));
    scratch.remove(src);
    match copied {
        Ok(_) => Ok(()),
        // A cancellation is not a failure of this file: left as it is.
        Err(e) if crate::error::io_carries_cancellation(&e) => Err(e.into()),
        Err(e) => {
            Err(crate::Error::from(e)
                .context(format!("failed to copy scratch file {}", src.display())))
        }
    }
}
