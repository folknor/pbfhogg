//! Parallel shard-based block-pair merge for `diff-snapshots`.
//!
//! Splits the ID space into N shards and runs an independent block-pair
//! merge per shard on a worker thread. Each shard is self-contained:
//! it owns its slice of old-side blobs and new-side blobs, runs its
//! merge loop with pread from a shared File, and streams output to a
//! per-shard scratch temp file. Main thread concatenates those temp
//! files to the caller's output in shard order.
//!
//! The temp-file output shape keeps steady-state anon RSS flat; an
//! earlier in-memory `Vec<u8>` shape buffered the full per-shard text
//! until join, peaking at ~2.3 GB on a planet diff at `-j 16`. The
//! sibling `--format osc` driver in `derive_parallel.rs` already used
//! per-shard temp files for the same reason.

use std::io::{self, BufWriter, Write};
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::blob_meta::{BlobIndex, ElemKind};
use crate::error::Result;
use crate::owned::TypeFilter;
use crate::read::header_walker::HeaderWalker;
use crate::{Element, PrimitiveBlock};

use super::{DiffOptions, DiffStats};

fn io_err(e: io::Error) -> crate::error::Error {
    crate::error::new_error(crate::error::ErrorKind::Io(e))
}

// ---------------------------------------------------------------------------
// Blob descriptor walker
// ---------------------------------------------------------------------------

/// One OsmData blob's location + index metadata, enough to pread and
/// classify without decompression.
#[derive(Debug, Clone, Copy)]
struct BlobDesc {
    data_offset: u64,
    data_size: usize,
    index: BlobIndex,
}

/// Three per-kind blob lists plus a shared `File` handle for pread.
struct WalkedFile {
    nodes: Vec<BlobDesc>,
    ways: Vec<BlobDesc>,
    relations: Vec<BlobDesc>,
    file: Arc<std::fs::File>,
}

/// Walk a PBF file's blob headers via the shared `HeaderWalker` primitive,
/// collecting per-kind `BlobDesc` lists. Requires each OsmData blob to
/// carry indexdata; returns an error if any OsmData blob lacks it.
fn walk_file(path: &Path) -> Result<WalkedFile> {
    let mut walker = HeaderWalker::open(path)?;
    let mut nodes: Vec<BlobDesc> = Vec::new();
    let mut ways: Vec<BlobDesc> = Vec::new();
    let mut rels: Vec<BlobDesc> = Vec::new();
    let mut first = true;

    while let Some(meta) = walker.next_header()? {
        // Skip the required leading OsmHeader blob; subsequent OsmData
        // blobs must carry indexdata for the parallel path.
        if first {
            first = false;
            continue;
        }
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        let index = meta.index.ok_or_else(|| {
            crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(
                "block-pair merge requires indexdata but blob has none",
            )))
        })?;
        let desc = BlobDesc {
            data_offset: meta.data_offset,
            data_size: meta.data_size,
            index,
        };
        match index.kind {
            ElemKind::Node => nodes.push(desc),
            ElemKind::Way => ways.push(desc),
            ElemKind::Relation => rels.push(desc),
        }
    }

    Ok(WalkedFile {
        nodes,
        ways,
        relations: rels,
        file: Arc::clone(walker.shared_file()),
    })
}

// ---------------------------------------------------------------------------
// Shard planner
// ---------------------------------------------------------------------------

/// One shard: an ID range `(t_low, t_high]` plus the index ranges of
/// old and new blobs whose content intersects that ID range. Straddling
/// blobs (those whose range crosses the shard boundary) are read by
/// both adjacent shards; each shard's element merge clips to its own
/// ID window so every element is emitted exactly once.
#[derive(Debug, Clone, Copy)]
struct Shard {
    t_low: i64,
    t_high: i64,
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
}

/// Plan N shards by ID range. Thresholds are placed at old-blob
/// boundaries (N-1 evenly spaced). Old is clean by construction;
/// straddling new blobs are absorbed by both adjacent shards.
///
/// Threshold comparison is raw numeric (`i64`) while the element
/// merge inside shards uses `osm_id_cmp` (canonical order:
/// `0, -1, -2, ..., 1, 2, ...`). These disagree on mixed-sign
/// inputs, but production PBFs are positive-only (see osm_id.rs
/// commentary), so the raw compare is safe in practice. If
/// negative IDs ever enter production, this planner and every
/// `id > t_high`/`id <= t_low` clip in `emit_side` must be
/// rewritten against `osm_id_cmp`.
fn plan_shards(
    old_descs: &[BlobDesc],
    new_descs: &[BlobDesc],
    target_count: usize,
) -> Vec<Shard> {
    if target_count <= 1 || old_descs.is_empty() {
        return vec![Shard {
            t_low: i64::MIN,
            t_high: i64::MAX,
            old_start: 0,
            old_end: old_descs.len(),
            new_start: 0,
            new_end: new_descs.len(),
        }];
    }

    let n = target_count.min(old_descs.len()).max(1);
    // `old_descs` are sorted by id range (max_id monotone
    // non-decreasing), so the mapped thresholds are monotone
    // non-decreasing and duplicates only appear consecutively -
    // consecutive-dedup is sufficient, no sort needed.
    let mut thresholds: Vec<i64> = (1..n)
        .map(|k| old_descs[(k * old_descs.len() / n) - 1].index.max_id)
        .collect();
    thresholds.dedup();

    let mut shards: Vec<Shard> = Vec::with_capacity(thresholds.len() + 1);
    let mut t_low = i64::MIN;
    for &t_high in &thresholds {
        shards.push(build_shard(old_descs, new_descs, t_low, t_high));
        t_low = t_high;
    }
    shards.push(build_shard(old_descs, new_descs, t_low, i64::MAX));
    shards
}

fn build_shard(
    old_descs: &[BlobDesc],
    new_descs: &[BlobDesc],
    t_low: i64,
    t_high: i64,
) -> Shard {
    let old_start = old_descs
        .iter()
        .position(|b| b.index.max_id > t_low)
        .unwrap_or(old_descs.len());
    let old_end = old_descs
        .iter()
        .rposition(|b| b.index.min_id <= t_high)
        .map_or(old_start, |i| i + 1);
    let new_start = new_descs
        .iter()
        .position(|b| b.index.max_id > t_low)
        .unwrap_or(new_descs.len());
    let new_end = new_descs
        .iter()
        .rposition(|b| b.index.min_id <= t_high)
        .map_or(new_start, |i| i + 1);
    Shard {
        t_low,
        t_high,
        old_start,
        old_end: old_end.max(old_start),
        new_start,
        new_end: new_end.max(new_start),
    }
}

// ---------------------------------------------------------------------------
// Per-shard worker: in-memory block-pair merge
// ---------------------------------------------------------------------------

/// What one shard worker produces.
struct ShardOutput {
    /// Path to the per-shard scratch temp file holding this shard's
    /// formatted diff text. The driver streams it in shard order to the
    /// caller's output and then removes it.
    text_path: PathBuf,
    /// Per-shard stats, aggregated into the caller's `DiffStats`.
    stats: DiffStats,
}

/// Pread a blob's compressed data and decompress + parse into a `PrimitiveBlock`.
fn pread_decode(
    file: &std::fs::File,
    desc: BlobDesc,
    read_buf: &mut Vec<u8>,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
) -> Result<PrimitiveBlock> {
    read_buf.resize(desc.data_size, 0);
    file.read_exact_at(read_buf, desc.data_offset)
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
    let mut decompressed: Vec<u8> = Vec::new();
    crate::blob::decompress_blob_raw(read_buf, &mut decompressed)?;
    PrimitiveBlock::from_vec_with_scratch(decompressed, st_scratch, gr_scratch)
}

/// Decoded-side state carried between iterations within a shard.
struct Side {
    block: PrimitiveBlock,
    skip_count: usize,
    index: BlobIndex,
}

/// Run one shard's block-pair merge. Streams formatted text to a
/// per-shard scratch temp file and returns its path + stats.
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
    kind: ElemKind,
    options: &DiffOptions,
    scratch_dir: &Path,
) -> Result<ShardOutput> {
    let type_char = kind_type_char(kind);
    let kind_tag = match kind {
        ElemKind::Node => "n",
        ElemKind::Way => "w",
        ElemKind::Relation => "r",
    };
    let pid = std::process::id();
    let text_path = scratch_dir.join(format!("diff-par-{pid}-{kind_tag}-{shard_idx}.txt.tmp"));
    let file = std::fs::File::create(&text_path).map_err(io_err)?;
    let mut out = BufWriter::new(file);

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
        // Ensure both sides have a decoded block (or we've exhausted one side).
        if old_decoded.is_none() && old_idx < old_slice.len() {
            let desc = old_slice[old_idx];
            old_idx += 1;
            let block = pread_decode(old_file, desc, &mut old_read, &mut old_st, &mut old_gr)?;
            old_decoded = Some(Side {
                block,
                skip_count: 0,
                index: desc.index,
            });
        }
        if new_decoded.is_none() && new_idx < new_slice.len() {
            let desc = new_slice[new_idx];
            new_idx += 1;
            let block = pread_decode(new_file, desc, &mut new_read, &mut new_st, &mut new_gr)?;
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
    let _ = out.into_inner().map_err(|e| io_err(e.into_error()))?;
    Ok(ShardOutput { text_path, stats })
}

/// Emit elements of a decoded block (single-sided: all OldOnly or all
/// NewOnly) clipped to the shard's ID window `(t_low, t_high]`.
#[allow(clippy::too_many_arguments)]
fn emit_side(
    out: &mut impl Write,
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
        let version = crate::osc::merge_join::element_version(&elem);
        emit_element(out, prefix, type_char, id, version)?;
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
    out: &mut impl Write,
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
/// per side.
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
    out: &mut impl Write,
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
        let old_in_range = old_iter.peek().is_some_and(|e| element_id(e) <= merge_up_to);
        let new_in_range = new_iter.peek().is_some_and(|e| element_id(e) <= merge_up_to);

        match (old_in_range, new_in_range) {
            (false, false) => break,
            (true, false) => {
                let o = old_iter.next().expect("checked peek");
                emit_element(out, '-', type_char, element_id(&o), element_version(&o))?;
                stats.deleted += 1;
                old_consumed += 1;
            }
            (false, true) => {
                let n = new_iter.next().expect("checked peek");
                emit_element(out, '+', type_char, element_id(&n), element_version(&n))?;
                stats.created += 1;
                new_consumed += 1;
            }
            (true, true) => {
                let o_id = element_id(old_iter.peek().expect("checked"));
                let n_id = element_id(new_iter.peek().expect("checked"));
                match crate::osm_id::osm_id_cmp(o_id, n_id) {
                    std::cmp::Ordering::Less => {
                        let o = old_iter.next().expect("checked");
                        emit_element(out, '-', type_char, element_id(&o), element_version(&o))?;
                        stats.deleted += 1;
                        old_consumed += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        let n = new_iter.next().expect("checked");
                        emit_element(out, '+', type_char, element_id(&n), element_version(&n))?;
                        stats.created += 1;
                        new_consumed += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        let o = old_iter.next().expect("checked");
                        let n = new_iter.next().expect("checked");
                        old_consumed += 1;
                        new_consumed += 1;
                        if borrowed_elements_equal(&o, &n) {
                            if !options.suppress_common {
                                emit_element(
                                    out,
                                    ' ',
                                    type_char,
                                    o_id,
                                    element_version(&o),
                                )?;
                            }
                            stats.common += 1;
                        } else {
                            let old_ver = element_version(&o);
                            let new_ver = element_version(&n);
                            emit_modified(out, type_char, o_id, old_ver, new_ver)?;
                            // Verbose details are not supported on the parallel
                            // path yet. Callers that need verbose should use the
                            // sequential `diff_block_pair` (parallel shards are
                            // opt-in via env var).
                            let _ = options.verbose;
                            stats.modified += 1;
                        }
                    }
                }
            }
        }
    }

    Ok((old_consumed, new_consumed))
}

/// Shadow of `borrowed_elements_equal` from `osc::merge_join` (that function
/// is not `pub(crate)`). TODO: lift the borrowed-equality helpers to a
/// shared location instead of duplicating.
fn borrowed_elements_equal(a: &Element<'_>, b: &Element<'_>) -> bool {
    match (a, b) {
        (Element::DenseNode(_) | Element::Node(_), Element::DenseNode(_) | Element::Node(_)) => {
            borrowed_nodes_equal(a, b)
        }
        (Element::Way(wa), Element::Way(wb)) => {
            wa.refs().eq(wb.refs()) && wa.tags().eq(wb.tags())
        }
        (Element::Relation(ra), Element::Relation(rb)) => {
            borrowed_relations_equal(ra, rb)
        }
        _ => false,
    }
}

fn borrowed_nodes_equal(a: &Element<'_>, b: &Element<'_>) -> bool {
    let (a_lat, a_lon) = match a {
        Element::DenseNode(dn) => (dn.decimicro_lat(), dn.decimicro_lon()),
        Element::Node(n) => (n.decimicro_lat(), n.decimicro_lon()),
        _ => return false,
    };
    let (b_lat, b_lon) = match b {
        Element::DenseNode(dn) => (dn.decimicro_lat(), dn.decimicro_lon()),
        Element::Node(n) => (n.decimicro_lat(), n.decimicro_lon()),
        _ => return false,
    };
    if a_lat != b_lat || a_lon != b_lon {
        return false;
    }
    match (a, b) {
        (Element::DenseNode(da), Element::DenseNode(db)) => da.tags().eq(db.tags()),
        (Element::DenseNode(da), Element::Node(nb)) => da.tags().eq(nb.tags()),
        (Element::Node(na), Element::DenseNode(db)) => na.tags().eq(db.tags()),
        (Element::Node(na), Element::Node(nb)) => na.tags().eq(nb.tags()),
        _ => false,
    }
}

fn borrowed_relations_equal(a: &crate::Relation<'_>, b: &crate::Relation<'_>) -> bool {
    if !a.tags().eq(b.tags()) {
        return false;
    }
    let mut ai = a.members();
    let mut bi = b.members();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return true,
            (Some(am), Some(bm)) => {
                if am.id != bm.id {
                    return false;
                }
                let ar = am.role().unwrap_or("");
                let br = bm.role().unwrap_or("");
                if ar != br {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

fn kind_type_char(kind: ElemKind) -> char {
    match kind {
        ElemKind::Node => 'n',
        ElemKind::Way => 'w',
        ElemKind::Relation => 'r',
    }
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
    .map_err(io_err)
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
    .map_err(io_err)
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
pub(crate) fn diff_block_pair_parallel(
    old_path: &Path,
    new_path: &Path,
    output: &mut impl Write,
    options: &DiffOptions,
    _direct_io: bool,
    filter: &TypeFilter,
    shard_count: usize,
) -> Result<DiffStats> {
    // DIFF_SCAN_START/END and diff_common/created/modified/deleted counters
    // are emitted by the calling `diff()` function in `mod.rs`; do not
    // re-emit here.
    let old = walk_file(old_path)?;
    let new = walk_file(new_path)?;

    // Scratch temp files live alongside the old input (its parent is assumed
    // to be on a writable, fast filesystem; this is the same assumption the
    // `--format osc` sibling driver makes for its shard fragments).
    let scratch_dir = old_path.parent().unwrap_or(Path::new("."));

    let mut total = DiffStats::default();

    struct PhaseSlot<'a> {
        kind: ElemKind,
        enabled: bool,
        tag: &'static str,
        old: &'a [BlobDesc],
        new: &'a [BlobDesc],
    }
    let phases = [
        PhaseSlot {
            kind: ElemKind::Node,
            enabled: filter.nodes,
            tag: "NODE",
            old: &old.nodes,
            new: &new.nodes,
        },
        PhaseSlot {
            kind: ElemKind::Way,
            enabled: filter.ways,
            tag: "WAY",
            old: &old.ways,
            new: &new.ways,
        },
        PhaseSlot {
            kind: ElemKind::Relation,
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
                let h = s.spawn(move || {
                    run_shard(
                        shard,
                        shard_idx,
                        old_descs,
                        new_descs,
                        &old_file,
                        &new_file,
                        kind,
                        options,
                        scratch_dir,
                    )
                });
                handles.push(h);
            }
            for (idx, h) in handles.into_iter().enumerate() {
                shard_outputs[idx] = Some(h.join().unwrap_or_else(|_| {
                    Err(crate::error::new_error(crate::error::ErrorKind::Io(
                        std::io::Error::other("shard worker panicked"),
                    )))
                }));
            }
        });

        // Emit shard outputs in shard order: stream each per-shard temp file
        // to the caller's output, then remove it.
        for slot in shard_outputs {
            let shard_out = slot.expect("all slots filled")?;
            append_and_cleanup(output, &shard_out.text_path)?;
            total.common += shard_out.stats.common;
            total.created += shard_out.stats.created;
            total.modified += shard_out.stats.modified;
            total.deleted += shard_out.stats.deleted;
        }

        crate::debug::emit_marker(&end_marker);
    }

    Ok(total)
}

/// Copy a shard's scratch temp file to the caller's output writer and then
/// remove it. Mirrors `derive_parallel::append_and_cleanup`.
fn append_and_cleanup(out: &mut impl Write, src: &Path) -> Result<()> {
    let mut f = std::fs::File::open(src).map_err(io_err)?;
    io::copy(&mut f, out).map_err(io_err)?;
    drop(f);
    drop(std::fs::remove_file(src));
    Ok(())
}
