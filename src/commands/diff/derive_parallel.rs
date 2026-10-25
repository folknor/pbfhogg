//! Parallel shard-based `derive_changes` (`diff --format osc`).
//!
//! Mirror of `src/commands/diff/parallel.rs` for the OSC-XML output
//! path. Each shard worker opens three per-shard scratch files
//! (creates / modifies / deletes), streams its XML fragments into
//! them, and returns paths + counts. The main thread concatenates
//! each action type's shard files in shard order into the three
//! outer temp files consumed by `assemble_osc`.
//!
//! Temp files rather than in-memory buffers because at planet scale
//! the aggregate OSC is ~30 GB (~149 M changes × ~200 B each) and
//! per-shard memory buffers would blow the memory envelope.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use quick_xml::Writer;

use crate::ElementKind;
use crate::blob_meta::BlobIndex;
use crate::error::Result;
use crate::osc::merge_join::element_id;
use crate::osc::write::write_element_xml;
use crate::{Element, PrimitiveBlock};

use super::derive::{DeriveChangesStats, extract_delete_info, write_delete_element};

use super::shard::{BlobDesc, Shard, plan_shards, walk_pair};

// ---------------------------------------------------------------------------
// Shard worker
// ---------------------------------------------------------------------------

/// Per-shard output: paths to this shard's three scratch files (creates,
/// modifies, deletes) plus element counts.
struct ShardOutput {
    creates_path: crate::scratch::ScratchPath,
    modifies_path: crate::scratch::ScratchPath,
    deletes_path: crate::scratch::ScratchPath,
    create_count: u64,
    modify_count: u64,
    delete_count: u64,
}

/// The three per-shard scratch paths `run_shard` writes.
fn shard_xml_paths(
    scratch: &crate::scratch::RunScratch,
    kind: ElementKind,
    shard_idx: usize,
) -> Result<(
    crate::scratch::ScratchPath,
    crate::scratch::ScratchPath,
    crate::scratch::ScratchPath,
)> {
    let kind_tag = match kind {
        ElementKind::Node => "n",
        ElementKind::Way => "w",
        ElementKind::Relation => "r",
    };
    Ok((
        scratch.file(&format!(
            "derive-par-creates-{kind_tag}-{shard_idx}.xml.tmp"
        ))?,
        scratch.file(&format!(
            "derive-par-modifies-{kind_tag}-{shard_idx}.xml.tmp"
        ))?,
        scratch.file(&format!(
            "derive-par-deletes-{kind_tag}-{shard_idx}.xml.tmp"
        ))?,
    ))
}

fn pread_decode(
    file: &File,
    source: &Arc<Path>,
    desc: BlobDesc,
    read_buf: &mut Vec<u8>,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
) -> Result<PrimitiveBlock> {
    use std::os::unix::fs::FileExt as _;
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

struct Side {
    block: PrimitiveBlock,
    skip_count: usize,
    index: BlobIndex,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_shard(
    shard: Shard,
    shard_idx: usize,
    old_descs: &[BlobDesc],
    new_descs: &[BlobDesc],
    old_file: &File,
    new_file: &File,
    old_source: &Arc<Path>,
    new_source: &Arc<Path>,
    kind: ElementKind,
    scratch: &crate::scratch::RunScratch,
    increment_version: bool,
    timestamp: Option<u64>,
    cancel: &crate::host::Cancel,
) -> Result<ShardOutput> {
    // Test-only: the fault plan's `derive_shard_panic`. Simulates a
    // mid-phase shard crash; the caller's `scope.spawn(...).join()`
    // turns the panic into an `Err("derive shard worker panicked: ..")`
    // and the post-join cleanup sweeps every possible per-shard
    // scratch file (creates / modifies / deletes). Release builds
    // compile this out entirely.
    #[cfg(feature = "test-hooks")]
    if cancel
        .faults()
        .derive_shard_panic
        .load(std::sync::atomic::Ordering::Relaxed)
        == shard_idx
    {
        panic!("test-hooks: derive_parallel shard {shard_idx} panicking");
    }

    let (cp, mp, dp) = shard_xml_paths(scratch, kind, shard_idx)?;

    let mut creates_w = Writer::new(BufWriter::new(crate::scratch::create_file(&cp)?));
    let mut modifies_w = Writer::new(BufWriter::new(crate::scratch::create_file(&mp)?));
    let mut deletes_w = Writer::new(BufWriter::new(crate::scratch::create_file(&dp)?));
    let mut coord_buf = String::new();

    let old_slice = &old_descs[shard.old_start..shard.old_end];
    let new_slice = &new_descs[shard.new_start..shard.new_end];
    let t_low = shard.t_low;
    let t_high = shard.t_high;

    let mut old_idx = 0usize;
    let mut new_idx = 0usize;
    let mut old_decoded: Option<Side> = None;
    let mut new_decoded: Option<Side> = None;

    let mut old_read: Vec<u8> = Vec::new();
    let mut new_read: Vec<u8> = Vec::new();
    let mut old_st: Vec<(u32, u32)> = Vec::new();
    let mut old_gr: Vec<(u32, u32)> = Vec::new();
    let mut new_st: Vec<(u32, u32)> = Vec::new();
    let mut new_gr: Vec<(u32, u32)> = Vec::new();

    let mut create_count = 0u64;
    let mut modify_count = 0u64;
    let mut delete_count = 0u64;

    loop {
        // Once per merge step (at most one blob decoded per side); shards are
        // independent threads joined by the driver.
        cancel.check()?;
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
                for elem in os.block.elements().skip(os.skip_count) {
                    let id = element_id(&elem);
                    if id <= t_low {
                        continue;
                    }
                    if id > t_high {
                        break;
                    }
                    emit_delete(&mut deletes_w, &elem, kind, increment_version, timestamp)?;
                    delete_count += 1;
                }
            }
            (None, Some(_)) => {
                let ns = new_decoded.take().expect("checked");
                for elem in ns.block.elements().skip(ns.skip_count) {
                    let id = element_id(&elem);
                    if id <= t_low {
                        continue;
                    }
                    if id > t_high {
                        break;
                    }
                    emit_create(&mut creates_w, &elem, &mut coord_buf)?;
                    create_count += 1;
                }
            }
            (Some(os), Some(ns)) => {
                if os.index.max_id < ns.index.min_id {
                    let os = old_decoded.take().expect("checked");
                    for elem in os.block.elements().skip(os.skip_count) {
                        let id = element_id(&elem);
                        if id <= t_low {
                            continue;
                        }
                        if id > t_high {
                            break;
                        }
                        emit_delete(&mut deletes_w, &elem, kind, increment_version, timestamp)?;
                        delete_count += 1;
                    }
                    continue;
                }
                if ns.index.max_id < os.index.min_id {
                    let ns = new_decoded.take().expect("checked");
                    for elem in ns.block.elements().skip(ns.skip_count) {
                        let id = element_id(&elem);
                        if id <= t_low {
                            continue;
                        }
                        if id > t_high {
                            break;
                        }
                        emit_create(&mut creates_w, &elem, &mut coord_buf)?;
                        create_count += 1;
                    }
                    continue;
                }
                merge_overlapping_pair(
                    &mut old_decoded,
                    &mut new_decoded,
                    kind,
                    t_low,
                    t_high,
                    &mut creates_w,
                    &mut modifies_w,
                    &mut deletes_w,
                    &mut coord_buf,
                    &mut create_count,
                    &mut modify_count,
                    &mut delete_count,
                    increment_version,
                    timestamp,
                )?;
            }
        }
    }

    // Flush the three BufWriter<File>s.
    creates_w.get_mut().flush()?;
    modifies_w.get_mut().flush()?;
    deletes_w.get_mut().flush()?;

    Ok(ShardOutput {
        creates_path: cp,
        modifies_path: mp,
        deletes_path: dp,
        create_count,
        modify_count,
        delete_count,
    })
}

#[allow(clippy::too_many_arguments)]
fn merge_overlapping_pair<W: Write>(
    old_decoded: &mut Option<Side>,
    new_decoded: &mut Option<Side>,
    kind: ElementKind,
    t_low: i64,
    t_high: i64,
    creates_w: &mut Writer<W>,
    modifies_w: &mut Writer<W>,
    deletes_w: &mut Writer<W>,
    coord_buf: &mut String,
    create_count: &mut u64,
    modify_count: &mut u64,
    delete_count: &mut u64,
    increment_version: bool,
    timestamp: Option<u64>,
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
        kind,
        creates_w,
        modifies_w,
        deletes_w,
        coord_buf,
        create_count,
        modify_count,
        delete_count,
        increment_version,
        timestamp,
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn element_merge<W: Write>(
    old_block: &PrimitiveBlock,
    old_skip: usize,
    new_block: &PrimitiveBlock,
    new_skip: usize,
    merge_up_to: i64,
    t_low: i64,
    kind: ElementKind,
    creates_w: &mut Writer<W>,
    modifies_w: &mut Writer<W>,
    deletes_w: &mut Writer<W>,
    coord_buf: &mut String,
    create_count: &mut u64,
    modify_count: &mut u64,
    delete_count: &mut u64,
    increment_version: bool,
    timestamp: Option<u64>,
) -> Result<(usize, usize)> {
    let mut old_iter = old_block.elements().skip(old_skip).peekable();
    let mut new_iter = new_block.elements().skip(new_skip).peekable();
    let mut old_consumed = 0usize;
    let mut new_consumed = 0usize;

    // Consume elements whose ID falls at or below `t_low` - those
    // belong to the previous shard. `old_consumed` / `new_consumed`
    // still tracks how many elements we advanced past, so the caller
    // can update its skip_count correctly if this blob remains as a
    // residual for the next pair within the shard.
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
                emit_delete(deletes_w, &o, kind, increment_version, timestamp)?;
                *delete_count += 1;
                old_consumed += 1;
            }
            (false, true) => {
                let n = new_iter.next().expect("checked peek");
                emit_create(creates_w, &n, coord_buf)?;
                *create_count += 1;
                new_consumed += 1;
            }
            (true, true) => {
                let o_id = element_id(old_iter.peek().expect("checked"));
                let n_id = element_id(new_iter.peek().expect("checked"));
                match crate::osm_id::osm_id_cmp(o_id, n_id) {
                    std::cmp::Ordering::Less => {
                        let o = old_iter.next().expect("checked");
                        emit_delete(deletes_w, &o, kind, increment_version, timestamp)?;
                        *delete_count += 1;
                        old_consumed += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        let n = new_iter.next().expect("checked");
                        emit_create(creates_w, &n, coord_buf)?;
                        *create_count += 1;
                        new_consumed += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        let o = old_iter.next().expect("checked");
                        let n = new_iter.next().expect("checked");
                        old_consumed += 1;
                        new_consumed += 1;
                        if !crate::content::content_equal(&o, &n) {
                            emit_create(modifies_w, &n, coord_buf)?;
                            *modify_count += 1;
                        }
                    }
                }
            }
        }
    }

    Ok((old_consumed, new_consumed))
}

// ---------------------------------------------------------------------------
// Emit helpers
// ---------------------------------------------------------------------------

fn emit_create<W: Write>(
    writer: &mut Writer<W>,
    elem: &Element<'_>,
    coord_buf: &mut String,
) -> Result<()> {
    write_element_xml(writer, elem, coord_buf)
}

fn emit_delete<W: Write>(
    writer: &mut Writer<W>,
    elem: &Element<'_>,
    kind: ElementKind,
    increment_version: bool,
    timestamp: Option<u64>,
) -> Result<()> {
    let Some((tag, id, meta)) = extract_delete_info(elem, kind) else {
        return Ok(());
    };
    write_delete_element(writer, tag, id, meta.as_ref(), increment_version, timestamp)
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Parallel variant of `derive_changes_block_pair`. Shards the ID space,
/// runs each shard on a worker thread emitting per-shard OSC XML temp
/// files, then concatenates them into the three outer temp files that
/// `assemble_osc` consumes.
///
/// Returns `Ok(None)`, having written nothing, when either input holds a
/// negative ID: the caller must run the sequential path instead (see
/// `shard`'s correctness contract).
///
/// Each input is opened once here, from its pin, by the header walk; the
/// walker's descriptor is the one the shard workers pread.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn derive_changes_parallel(
    old_input: &crate::read::input::PinnedInput,
    new_input: &crate::read::input::PinnedInput,
    output: &crate::output::Output,
    scratch: &crate::scratch::RunScratch,
    shard_count: usize,
    increment_version: bool,
    timestamp: Option<u64>,
    host: &crate::host::Host,
) -> Result<Option<DeriveChangesStats>> {
    let Some((old, new)) = walk_pair(old_input, new_input, host)? else {
        crate::host::diag!(
            host.diagnostics,
            Note,
            "derive_changes.negative_id_fallback",
            "Using sequential comparison: negative IDs cannot use parallel ID-range shards"
        );
        crate::debug::emit_counter("derive_changes_negative_id_fallback", 1);
        return Ok(None);
    };

    // Scratch peaks include completed outer action files plus the current
    // kind's shard fragments. Appending temporarily duplicates the source
    // shard, then removes it; gzip later keeps all three outer files alive.
    // Header ranges/counts cannot predict changed XML bytes, so an input-size
    // free-space refusal here would be a guess, not a lifetime-aware budget.
    // An exact estimate requires repeating the measured serialization pass;
    // retain bounded-memory streaming and propagate actual write failures.
    // Outer temp files that `assemble_osc_from_paths` consumes. A failure
    // anywhere leaves them to the run directory's guard.
    let outer_creates = scratch.file("derive-par-creates.xml.tmp")?;
    let outer_modifies = scratch.file("derive-par-modifies.xml.tmp")?;
    let outer_deletes = scratch.file("derive-par-deletes.xml.tmp")?;

    let mut totals = DeriveChangesStats {
        creates: 0,
        modifies: 0,
        deletes: 0,
    };

    // Open outer temp files up front; each phase appends its shard outputs
    // in order.
    let mut outer_creates_w = BufWriter::new(crate::scratch::create_file(&outer_creates)?);
    let mut outer_modifies_w = BufWriter::new(crate::scratch::create_file(&outer_modifies)?);
    let mut outer_deletes_w = BufWriter::new(crate::scratch::create_file(&outer_deletes)?);

    struct PhaseSlot<'a> {
        kind: ElementKind,
        tag: &'static str,
        /// Sidecar counter names: shards, shard_max_blobs, shard_min_blobs.
        counters: [&'static str; 3],
        old: &'a [BlobDesc],
        new: &'a [BlobDesc],
    }
    let phases = [
        PhaseSlot {
            kind: ElementKind::Node,
            tag: "NODE",
            counters: [
                "derivepar_node_shards",
                "derivepar_node_shard_max_blobs",
                "derivepar_node_shard_min_blobs",
            ],
            old: &old.nodes,
            new: &new.nodes,
        },
        PhaseSlot {
            kind: ElementKind::Way,
            tag: "WAY",
            counters: [
                "derivepar_way_shards",
                "derivepar_way_shard_max_blobs",
                "derivepar_way_shard_min_blobs",
            ],
            old: &old.ways,
            new: &new.ways,
        },
        PhaseSlot {
            kind: ElementKind::Relation,
            tag: "REL",
            counters: [
                "derivepar_rel_shards",
                "derivepar_rel_shard_max_blobs",
                "derivepar_rel_shard_min_blobs",
            ],
            old: &old.relations,
            new: &new.relations,
        },
    ];

    for phase in phases {
        if phase.old.is_empty() && phase.new.is_empty() {
            continue;
        }
        let kind = phase.kind;
        let tag = phase.tag;
        let start_marker = format!("DERIVECHANGES_PHASE_{tag}_START");
        let end_marker = format!("DERIVECHANGES_PHASE_{tag}_END");
        crate::debug::emit_marker(&start_marker);

        let shards = plan_shards(phase.old, phase.new, shard_count);
        let [shards_name, max_name, min_name] = phase.counters;
        crate::debug::emit_counter(shards_name, shards.len());
        let max_blobs = shards
            .iter()
            .map(|s| (s.old_end - s.old_start) + (s.new_end - s.new_start))
            .max()
            .unwrap_or(0);
        let min_blobs = shards
            .iter()
            .map(|s| (s.old_end - s.old_start) + (s.new_end - s.new_start))
            .min()
            .unwrap_or(0);
        crate::debug::emit_counter(max_name, max_blobs);
        crate::debug::emit_counter(min_name, min_blobs);
        let mut shard_outputs: Vec<Option<Result<ShardOutput>>> =
            (0..shards.len()).map(|_| None).collect();

        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(shards.len());
            for (idx, shard) in shards.iter().copied().enumerate() {
                let old_file = Arc::clone(&old.file);
                let new_file = Arc::clone(&new.file);
                let old_source = &old.path;
                let new_source = &new.path;
                let old_descs = phase.old;
                let new_descs = phase.new;
                let h = s.spawn(move || {
                    run_shard(
                        shard,
                        idx,
                        old_descs,
                        new_descs,
                        &old_file,
                        &new_file,
                        old_source,
                        new_source,
                        kind,
                        scratch,
                        increment_version,
                        timestamp,
                        &host.cancel,
                    )
                });
                handles.push(h);
            }
            for (idx, h) in handles.into_iter().enumerate() {
                shard_outputs[idx] = Some(h.join().unwrap_or_else(|payload| {
                    Err(crate::error::panic_error(
                        "derive shard worker",
                        payload.as_ref(),
                    ))
                }));
            }
        });

        // Partition shard outputs into (successful, first error). On an error
        // every shard file is left for the run directory's guard. Shards run
        // independently, so slot order is not time order: a real failure in
        // any shard outranks the cancellation the others observed.
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

        // All successful: concatenate this phase's per-shard outputs into
        // the three outer temp files in shard order, then delete the
        // per-shard files.
        append_shard_outputs(
            &mut outer_creates_w,
            &mut outer_modifies_w,
            &mut outer_deletes_w,
            outputs,
            &mut totals,
            scratch,
            &host.cancel,
        )?;

        crate::debug::emit_marker(&end_marker);
    }

    outer_creates_w.flush()?;
    outer_modifies_w.flush()?;
    outer_deletes_w.flush()?;
    drop(outer_creates_w);
    drop(outer_modifies_w);
    drop(outer_deletes_w);

    crate::debug::emit_marker("DERIVECHANGES_WRITE_START");
    super::derive::assemble_osc_from_paths(
        output,
        &outer_creates,
        &outer_modifies,
        &outer_deletes,
        totals.creates,
        totals.modifies,
        totals.deletes,
        host,
    )?;
    crate::debug::emit_marker("DERIVECHANGES_WRITE_END");

    // The outer files are the largest scratch of the run; give the space back
    // before the caller does anything else.
    scratch.remove(&outer_creates);
    scratch.remove(&outer_modifies);
    scratch.remove(&outer_deletes);

    Ok(Some(totals))
}

/// Concatenate every shard's three scratch files onto the outer temp files in
/// shard order, folding each shard's counts into `totals`.
///
/// A failed append stops the emission, but every remaining scratch file is
/// still removed before the error returns. Returning on the first error left
/// the rest of the shards' files (three per shard) in the scratch directory.
fn append_shard_outputs(
    creates_w: &mut BufWriter<File>,
    modifies_w: &mut BufWriter<File>,
    deletes_w: &mut BufWriter<File>,
    outputs: Vec<ShardOutput>,
    totals: &mut DeriveChangesStats,
    scratch: &crate::scratch::RunScratch,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    let mut first_err = None;
    for shard_out in outputs {
        // Once per shard, between appends (the appends themselves are kernel
        // copies with no checkpoint): after a cancellation the remaining
        // shard files are removed below, as after a failed append.
        if first_err.is_none()
            && let Err(e) = cancel.check()
        {
            first_err = Some(e);
        }
        if first_err.is_some() {
            scratch.remove(&shard_out.creates_path);
            scratch.remove(&shard_out.modifies_path);
            scratch.remove(&shard_out.deletes_path);
            continue;
        }
        // `and` takes its argument eagerly, so all three calls run and each
        // removes its own source file even when an earlier copy failed; the
        // first error is the one kept.
        let appended = append_and_cleanup(creates_w, &shard_out.creates_path, scratch)
            .and(append_and_cleanup(
                modifies_w,
                &shard_out.modifies_path,
                scratch,
            ))
            .and(append_and_cleanup(
                deletes_w,
                &shard_out.deletes_path,
                scratch,
            ));
        if let Err(e) = appended {
            first_err = Some(e);
            continue;
        }
        totals.creates += shard_out.create_count;
        totals.modifies += shard_out.modify_count;
        totals.deletes += shard_out.delete_count;
    }
    first_err.map_or(Ok(()), Err)
}

/// Copy a shard's scratch file onto `out` and then remove it - also when the
/// copy fails, so a failed write does not leave the temp file behind.
fn append_and_cleanup(
    out: &mut BufWriter<File>,
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
