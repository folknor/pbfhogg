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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use quick_xml::Writer;

use crate::blob_meta::{BlobIndex, ElemKind};
use crate::error::Result;
use crate::osc::merge_join::{element_id, element_version};
use crate::osc::write::{write_element_xml, OwnedMetadata};
use crate::read::header_walker::HeaderWalker;
use crate::{Element, PrimitiveBlock};

use super::derive::DeriveChangesStats;

// ---------------------------------------------------------------------------
// Shard planning (duplicated from parallel.rs for now; will be unified in a
// follow-up commit that hoists walk_file + plan_shards into a shared module)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct BlobDesc {
    data_offset: u64,
    data_size: usize,
    index: BlobIndex,
}

struct WalkedFile {
    nodes: Vec<BlobDesc>,
    ways: Vec<BlobDesc>,
    relations: Vec<BlobDesc>,
    file: Arc<File>,
}

fn walk_file(path: &Path) -> Result<WalkedFile> {
    let mut walker = HeaderWalker::open(path)?;
    let mut nodes: Vec<BlobDesc> = Vec::new();
    let mut ways: Vec<BlobDesc> = Vec::new();
    let mut rels: Vec<BlobDesc> = Vec::new();
    let mut first = true;

    while let Some(meta) = walker.next_header()? {
        if first {
            // Skip the leading OsmHeader blob; subsequent OsmData blobs
            // must carry indexdata for this path.
            first = false;
            continue;
        }
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        let Some(index) = meta.index else {
            return Err(crate::error::new_error(crate::error::ErrorKind::Io(
                io::Error::other(
                    "derive_changes parallel path requires indexdata but a blob has none",
                ),
            )));
        };
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

#[derive(Debug, Clone, Copy)]
struct Shard {
    /// IDs strictly greater than this belong to this shard.
    /// `i64::MIN` for the first shard.
    t_low: i64,
    /// IDs less than or equal to this belong to this shard.
    /// `i64::MAX` for the last shard.
    t_high: i64,
    /// Old-side blobs whose ID ranges intersect `(t_low, t_high]`.
    /// A blob that straddles a shard boundary is included in both
    /// adjacent shards; each one processes only the portion inside
    /// its `(t_low, t_high]` window.
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
}

/// Plan N shards by ID range. Thresholds are placed at old-blob
/// boundaries so correctness doesn't depend on new-side alignment;
/// straddling new blobs are read by both adjacent shards and each
/// shard's element merge filters to its `(t_low, t_high]` window.
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

    // Place N-1 thresholds at evenly-spaced old-blob boundaries.
    // Using old-blob max_ids guarantees the old side has clean breaks
    // (no old blob straddles a threshold by construction). New blobs
    // that do straddle are absorbed by both adjacent shards.
    let n = target_count.min(old_descs.len()).max(1);
    let mut thresholds: Vec<i64> = (1..n)
        .map(|k| old_descs[(k * old_descs.len() / n) - 1].index.max_id)
        .collect();
    // Ensure strictly increasing (consecutive old blobs could share a
    // fencepost if we rounded off the same blob index; in practice this
    // can only happen for tiny inputs where n is overkill).
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
    // A blob intersects (t_low, t_high] iff max_id > t_low and
    // min_id <= t_high. Because blobs are sorted by (max_id, min_id)
    // monotonically, the intersecting range is contiguous.
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
// Shard worker
// ---------------------------------------------------------------------------

/// Per-shard output: paths to this shard's three scratch files (creates,
/// modifies, deletes) plus element counts.
struct ShardOutput {
    creates_path: PathBuf,
    modifies_path: PathBuf,
    deletes_path: PathBuf,
    create_count: u64,
    modify_count: u64,
    delete_count: u64,
}

/// Process-lifetime random tag for scratch filenames. PID alone collides
/// when two `pbfhogg` processes share a PID in the same scratch dir
/// (container restart recycling PID); adding this tag dodges that.
fn process_scratch_tag() -> &'static str {
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TAG.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        #[allow(clippy::cast_possible_truncation)]
        let lo = nanos as u32;
        format!("{lo:08x}")
    })
}

/// Compute the three per-shard scratch paths that `run_shard` would
/// create. Exposed so the caller can clean up leftover files from
/// panicked / errored shards whose `ShardOutput` is unavailable.
fn shard_xml_paths(
    scratch_dir: &Path,
    kind: ElemKind,
    shard_idx: usize,
) -> (PathBuf, PathBuf, PathBuf) {
    let pid = std::process::id();
    let tag = process_scratch_tag();
    let kind_tag = match kind {
        ElemKind::Node => "n",
        ElemKind::Way => "w",
        ElemKind::Relation => "r",
    };
    (
        scratch_dir.join(format!(
            "derive-par-creates-{pid}-{tag}-{kind_tag}-{shard_idx}.xml.tmp"
        )),
        scratch_dir.join(format!(
            "derive-par-modifies-{pid}-{tag}-{kind_tag}-{shard_idx}.xml.tmp"
        )),
        scratch_dir.join(format!(
            "derive-par-deletes-{pid}-{tag}-{kind_tag}-{shard_idx}.xml.tmp"
        )),
    )
}

fn pread_decode(
    file: &File,
    desc: BlobDesc,
    read_buf: &mut Vec<u8>,
    st_scratch: &mut Vec<(u32, u32)>,
    gr_scratch: &mut Vec<(u32, u32)>,
) -> Result<PrimitiveBlock> {
    use std::os::unix::fs::FileExt as _;
    read_buf.resize(desc.data_size, 0);
    file.read_exact_at(read_buf, desc.data_offset)
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
    let mut decompressed: Vec<u8> = Vec::new();
    crate::blob::decompress_blob_raw(read_buf, &mut decompressed)?;
    PrimitiveBlock::from_vec_with_scratch(decompressed, st_scratch, gr_scratch)
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
    kind: ElemKind,
    scratch_dir: &Path,
    increment_version: bool,
    update_timestamp: bool,
) -> Result<ShardOutput> {
    let (cp, mp, dp) = shard_xml_paths(scratch_dir, kind, shard_idx);

    let mut creates_w = Writer::new(BufWriter::new(
        File::create(&cp).map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?,
    ));
    let mut modifies_w = Writer::new(BufWriter::new(
        File::create(&mp).map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?,
    ));
    let mut deletes_w = Writer::new(BufWriter::new(
        File::create(&dp).map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?,
    ));
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
                for elem in os.block.elements().skip(os.skip_count) {
                    let id = element_id(&elem);
                    if id <= t_low {
                        continue;
                    }
                    if id > t_high {
                        break;
                    }
                    emit_delete(&mut deletes_w, &elem, kind, increment_version, update_timestamp)?;
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
                        emit_delete(&mut deletes_w, &elem, kind, increment_version, update_timestamp)?;
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
                    update_timestamp,
                )?;
            }
        }
    }

    // Flush the three BufWriter<File>s.
    creates_w
        .get_mut()
        .flush()
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
    modifies_w
        .get_mut()
        .flush()
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
    deletes_w
        .get_mut()
        .flush()
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;

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
    kind: ElemKind,
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
    update_timestamp: bool,
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
        update_timestamp,
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
    kind: ElemKind,
    creates_w: &mut Writer<W>,
    modifies_w: &mut Writer<W>,
    deletes_w: &mut Writer<W>,
    coord_buf: &mut String,
    create_count: &mut u64,
    modify_count: &mut u64,
    delete_count: &mut u64,
    increment_version: bool,
    update_timestamp: bool,
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
        let old_in_range = old_iter.peek().is_some_and(|e| element_id(e) <= merge_up_to);
        let new_in_range = new_iter.peek().is_some_and(|e| element_id(e) <= merge_up_to);

        match (old_in_range, new_in_range) {
            (false, false) => break,
            (true, false) => {
                let o = old_iter.next().expect("checked peek");
                emit_delete(deletes_w, &o, kind, increment_version, update_timestamp)?;
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
                        emit_delete(deletes_w, &o, kind, increment_version, update_timestamp)?;
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
                        if !borrowed_elements_equal(&o, &n) {
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

fn borrowed_elements_equal(a: &Element<'_>, b: &Element<'_>) -> bool {
    match (a, b) {
        (Element::DenseNode(_) | Element::Node(_), Element::DenseNode(_) | Element::Node(_)) => {
            borrowed_nodes_equal(a, b)
        }
        (Element::Way(wa), Element::Way(wb)) => {
            wa.refs().eq(wb.refs()) && wa.tags().eq(wb.tags())
        }
        (Element::Relation(ra), Element::Relation(rb)) => borrowed_relations_equal(ra, rb),
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

// ---------------------------------------------------------------------------
// Emit helpers - BoxResult -> crate::error::Result conversion for Send safety
// ---------------------------------------------------------------------------

#[allow(clippy::needless_pass_by_value)]
fn map_emit_err(e: Box<dyn std::error::Error>) -> crate::error::Error {
    crate::error::new_error(crate::error::ErrorKind::Io(io::Error::other(e.to_string())))
}

fn emit_create<W: Write>(
    writer: &mut Writer<W>,
    elem: &Element<'_>,
    coord_buf: &mut String,
) -> Result<()> {
    write_element_xml(writer, elem, coord_buf).map_err(map_emit_err)
}

fn emit_delete<W: Write>(
    writer: &mut Writer<W>,
    elem: &Element<'_>,
    kind: ElemKind,
    increment_version: bool,
    update_timestamp: bool,
) -> Result<()> {
    let Some((tag, id, meta)) = extract_delete_info(elem, kind) else {
        return Ok(());
    };
    write_delete_element(writer, tag, id, meta.as_ref(), increment_version, update_timestamp)
        .map_err(map_emit_err)
}

fn extract_delete_info(
    elem: &Element<'_>,
    kind: ElemKind,
) -> Option<(&'static str, i64, Option<OwnedMetadata>)> {
    match (kind, elem) {
        (ElemKind::Node, Element::DenseNode(dn)) => Some((
            "node",
            dn.id(),
            dn.info()
                .map(crate::dense::DenseNodeInfo::version)
                .filter(|&v| v != -1)
                .map(OwnedMetadata::version_only),
        )),
        (ElemKind::Node, Element::Node(n)) => Some((
            "node",
            n.id(),
            n.info().version().map(OwnedMetadata::version_only),
        )),
        (ElemKind::Way, Element::Way(w)) => Some((
            "way",
            w.id(),
            w.info().version().map(OwnedMetadata::version_only),
        )),
        (ElemKind::Relation, Element::Relation(r)) => Some((
            "relation",
            r.id(),
            r.info().version().map(OwnedMetadata::version_only),
        )),
        _ => None,
    }
}

fn write_delete_element<W: Write>(
    writer: &mut Writer<W>,
    tag_name: &str,
    id: i64,
    metadata: Option<&OwnedMetadata>,
    increment_version: bool,
    update_timestamp: bool,
) -> crate::BoxResult<()> {
    use quick_xml::events::{BytesStart, Event};
    let mut elem = BytesStart::new(tag_name);
    let id_str = id.to_string();
    elem.push_attribute(("id", id_str.as_str()));
    if let Some(meta) = metadata {
        let version = if increment_version {
            meta.version.saturating_add(1)
        } else {
            meta.version
        };
        let v_str = version.to_string();
        elem.push_attribute(("version", v_str.as_str()));
    }
    if update_timestamp {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let ts = crate::commands::format_epoch_secs(now.as_secs());
        elem.push_attribute(("timestamp", ts.as_str()));
    }
    writer.write_event(Event::Empty(elem))?;
    let _ = element_version; // silence "unused helper" if future paths drop it
    Ok(())
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Parallel variant of `derive_changes_block_pair`. Shards the ID space,
/// runs each shard on a worker thread emitting per-shard OSC XML temp
/// files, then concatenates them into the three outer temp files that
/// `assemble_osc` consumes.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn derive_changes_parallel(
    old_path: &Path,
    new_path: &Path,
    output: &Path,
    scratch_dir: &Path,
    shard_count: usize,
    increment_version: bool,
    update_timestamp: bool,
) -> Result<DeriveChangesStats> {
    let old = walk_file(old_path)?;
    let new = walk_file(new_path)?;

    // Outer temp files that `assemble_osc_raw` consumes.
    let pid = std::process::id();
    let outer_creates = scratch_dir.join(format!("derive-par-creates-{pid}.xml.tmp"));
    let outer_modifies = scratch_dir.join(format!("derive-par-modifies-{pid}.xml.tmp"));
    let outer_deletes = scratch_dir.join(format!("derive-par-deletes-{pid}.xml.tmp"));

    let mut totals = DeriveChangesStats {
        creates: 0,
        modifies: 0,
        deletes: 0,
    };

    // Open outer temp files up front; each phase appends its shard outputs
    // in order.
    let mut outer_creates_w = BufWriter::new(
        File::create(&outer_creates)
            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?,
    );
    let mut outer_modifies_w = BufWriter::new(
        File::create(&outer_modifies)
            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?,
    );
    let mut outer_deletes_w = BufWriter::new(
        File::create(&outer_deletes)
            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?,
    );

    struct PhaseSlot<'a> {
        kind: ElemKind,
        tag: &'static str,
        old: &'a [BlobDesc],
        new: &'a [BlobDesc],
    }
    let phases = [
        PhaseSlot {
            kind: ElemKind::Node,
            tag: "NODE",
            old: &old.nodes,
            new: &new.nodes,
        },
        PhaseSlot {
            kind: ElemKind::Way,
            tag: "WAY",
            old: &old.ways,
            new: &new.ways,
        },
        PhaseSlot {
            kind: ElemKind::Relation,
            tag: "REL",
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
        #[allow(clippy::cast_possible_wrap)]
        {
            crate::debug::emit_counter(
                &format!("derivepar_{}_shards", tag.to_lowercase()),
                shards.len() as i64,
            );
            let max_blobs = shards
                .iter()
                .map(|s| (s.old_end - s.old_start) + (s.new_end - s.new_start))
                .max()
                .unwrap_or(0) as i64;
            let min_blobs = shards
                .iter()
                .map(|s| (s.old_end - s.old_start) + (s.new_end - s.new_start))
                .min()
                .unwrap_or(0) as i64;
            crate::debug::emit_counter(
                &format!("derivepar_{}_shard_max_blobs", tag.to_lowercase()),
                max_blobs,
            );
            crate::debug::emit_counter(
                &format!("derivepar_{}_shard_min_blobs", tag.to_lowercase()),
                min_blobs,
            );
        }
        let mut shard_outputs: Vec<Option<Result<ShardOutput>>> =
            (0..shards.len()).map(|_| None).collect();

        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(shards.len());
            for (idx, shard) in shards.iter().copied().enumerate() {
                let old_file = Arc::clone(&old.file);
                let new_file = Arc::clone(&new.file);
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
                        kind,
                        scratch_dir,
                        increment_version,
                        update_timestamp,
                    )
                });
                handles.push(h);
            }
            for (idx, h) in handles.into_iter().enumerate() {
                shard_outputs[idx] = Some(h.join().unwrap_or_else(|_| {
                    Err(crate::error::new_error(crate::error::ErrorKind::Io(
                        io::Error::other("derive shard worker panicked"),
                    )))
                }));
            }
        });

        // Partition shard outputs into (successful, first error). If any
        // shard errored or panicked, clean up every possible per-shard
        // temp file - including ones that completed successfully as well
        // as any that a panicked worker left behind - before returning.
        // Without this pass, `scratch_dir` accumulates
        // `derive-par-{creates,modifies,deletes}-{pid}-*` on every failed
        // run.
        let mut outputs: Vec<ShardOutput> = Vec::with_capacity(shard_outputs.len());
        let mut first_err: Option<crate::error::Error> = None;
        for slot in shard_outputs {
            match slot.expect("all slots filled") {
                Ok(out) => outputs.push(out),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if let Some(e) = first_err {
            for out in &outputs {
                drop(std::fs::remove_file(&out.creates_path));
                drop(std::fs::remove_file(&out.modifies_path));
                drop(std::fs::remove_file(&out.deletes_path));
            }
            for shard_idx in 0..shards.len() {
                let (cp, mp, dp) = shard_xml_paths(scratch_dir, kind, shard_idx);
                drop(std::fs::remove_file(cp));
                drop(std::fs::remove_file(mp));
                drop(std::fs::remove_file(dp));
            }
            return Err(e);
        }

        // All successful: concatenate this phase's per-shard outputs into
        // the three outer temp files in shard order, then delete the
        // per-shard files.
        for shard_out in outputs {
            append_and_cleanup(&mut outer_creates_w, &shard_out.creates_path)?;
            append_and_cleanup(&mut outer_modifies_w, &shard_out.modifies_path)?;
            append_and_cleanup(&mut outer_deletes_w, &shard_out.deletes_path)?;
            totals.creates += shard_out.create_count;
            totals.modifies += shard_out.modify_count;
            totals.deletes += shard_out.delete_count;
        }

        crate::debug::emit_marker(&end_marker);
    }

    outer_creates_w
        .flush()
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
    outer_modifies_w
        .flush()
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
    outer_deletes_w
        .flush()
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
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
    )
    .map_err(map_emit_err)?;
    crate::debug::emit_marker("DERIVECHANGES_WRITE_END");

    // Clean up outer temp files.
    drop(std::fs::remove_file(&outer_creates));
    drop(std::fs::remove_file(&outer_modifies));
    drop(std::fs::remove_file(&outer_deletes));

    Ok(totals)
}

fn append_and_cleanup(out: &mut BufWriter<File>, src: &Path) -> Result<()> {
    let mut f = File::open(src)
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
    io::copy(&mut f, out)
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
    drop(f);
    drop(std::fs::remove_file(src));
    Ok(())
}
