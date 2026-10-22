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
use crate::osc::write::{OwnedMetadata, write_element_xml};
use crate::{Element, PrimitiveBlock};

use super::derive::DeriveChangesStats;

// Fault-injection hooks for tests. Gated behind the `static-test-hooks`
// Cargo feature and `not(test)`: product builds and the library's own
// unit-test harness never compile this module.
// Mirror of `parallel::test_hooks`. Could unify in the future when
// the shard planning infrastructure is hoisted into a shared
// module (see the duplication note below).
#[cfg(all(feature = "static-test-hooks", not(test)))]
pub mod test_hooks {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Shard index at which `run_shard` panics. `usize::MAX` =
    /// disarmed (default). Call [`reset`] at the start AND end of
    /// any test that arms this so sibling tests don't inherit state.
    pub static PANIC_AT_SHARD_IDX: AtomicUsize = AtomicUsize::new(usize::MAX);

    /// Disarm the hook.
    pub fn reset() {
        PANIC_AT_SHARD_IDX.store(usize::MAX, Ordering::Relaxed);
    }
}

use super::shard::{BlobDesc, Shard, plan_shards, walk_pair};

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
///
/// `suffix` is the run token's ([`crate::scratch::RunToken::suffix`]), so two
/// runs in one process - which share the pid and the process tag - name and
/// clean up disjoint files.
fn shard_xml_paths(
    scratch_dir: &Path,
    suffix: &str,
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
            "derive-par-creates-{pid}{suffix}-{tag}-{kind_tag}-{shard_idx}.xml.tmp"
        )),
        scratch_dir.join(format!(
            "derive-par-modifies-{pid}{suffix}-{tag}-{kind_tag}-{shard_idx}.xml.tmp"
        )),
        scratch_dir.join(format!(
            "derive-par-deletes-{pid}{suffix}-{tag}-{kind_tag}-{shard_idx}.xml.tmp"
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
    scratch_suffix: &str,
    increment_version: bool,
    update_timestamp: bool,
) -> Result<ShardOutput> {
    // Test-only: arm via `test_hooks::PANIC_AT_SHARD_IDX`. Simulates
    // a mid-phase shard crash; the caller's `scope.spawn(...).join()`
    // turns the panic into an `Err("derive shard worker panicked")`
    // and the post-join cleanup sweeps every possible per-shard
    // scratch file (creates / modifies / deletes). Release builds
    // compile this out entirely.
    #[cfg(all(feature = "static-test-hooks", not(test)))]
    {
        use std::sync::atomic::Ordering;
        if test_hooks::PANIC_AT_SHARD_IDX.load(Ordering::Relaxed) == shard_idx {
            panic!("test-hooks: derive_parallel shard {shard_idx} panicking");
        }
    }

    let (cp, mp, dp) = shard_xml_paths(scratch_dir, scratch_suffix, kind, shard_idx);

    let mut creates_w =
        Writer::new(BufWriter::new(File::create(&cp).map_err(|e| {
            crate::error::new_error(crate::error::ErrorKind::Io(e))
        })?));
    let mut modifies_w =
        Writer::new(BufWriter::new(File::create(&mp).map_err(|e| {
            crate::error::new_error(crate::error::ErrorKind::Io(e))
        })?));
    let mut deletes_w =
        Writer::new(BufWriter::new(File::create(&dp).map_err(|e| {
            crate::error::new_error(crate::error::ErrorKind::Io(e))
        })?));
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
                    emit_delete(
                        &mut deletes_w,
                        &elem,
                        kind,
                        increment_version,
                        update_timestamp,
                    )?;
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
                        emit_delete(
                            &mut deletes_w,
                            &elem,
                            kind,
                            increment_version,
                            update_timestamp,
                        )?;
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
    write_delete_element(
        writer,
        tag,
        id,
        meta.as_ref(),
        increment_version,
        update_timestamp,
    )
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
    // Emitted only when the source claimed a version. Absent is not
    // zero, and `version="0"` on a delete is a claim the input never
    // made.
    if let Some(version) = metadata.and_then(|m| m.version) {
        let version = if increment_version {
            version.saturating_add(1)
        } else {
            version
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
///
/// Returns `Ok(None)`, having written nothing, when either input holds a
/// negative ID: the caller must run the sequential path instead (see
/// `shard`'s correctness contract).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn derive_changes_parallel(
    old_path: &Path,
    new_path: &Path,
    output: &Path,
    scratch_dir: &Path,
    shard_count: usize,
    increment_version: bool,
    update_timestamp: bool,
    host: &crate::host::Host,
    token: &crate::scratch::RunToken,
) -> Result<Option<DeriveChangesStats>> {
    let Some((old, new)) = walk_pair(old_path, new_path, host)? else {
        return Ok(None);
    };
    let suffix = token.suffix();
    let suffix = suffix.as_str();

    // Outer temp files that `assemble_osc_raw` consumes. Wrap each in
    // a `PathGuard` (see `decisions/0003-*`) so any early return
    // (shard panic, sweep error, flush failure) removes them instead
    // of leaking into scratch. Guards are `commit()`ed after the
    // assemble step reads them, just before they'd be removed
    // manually anyway.
    let pid = std::process::id();
    let outer_creates = scratch_dir.join(format!("derive-par-creates-{pid}{suffix}.xml.tmp"));
    let outer_modifies = scratch_dir.join(format!("derive-par-modifies-{pid}{suffix}.xml.tmp"));
    let outer_deletes = scratch_dir.join(format!("derive-par-deletes-{pid}{suffix}.xml.tmp"));

    let mut totals = DeriveChangesStats {
        creates: 0,
        modifies: 0,
        deletes: 0,
    };

    // Open outer temp files up front; each phase appends its shard outputs
    // in order.
    let mut outer_creates_w = BufWriter::new(
        crate::scratch::create_file(&outer_creates)
            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?,
    );
    let outer_creates_guard = crate::path_guard::PathGuard::file(outer_creates.clone());
    let mut outer_modifies_w = BufWriter::new(
        crate::scratch::create_file(&outer_modifies)
            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?,
    );
    let outer_modifies_guard = crate::path_guard::PathGuard::file(outer_modifies.clone());
    let mut outer_deletes_w = BufWriter::new(
        crate::scratch::create_file(&outer_deletes)
            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?,
    );
    let outer_deletes_guard = crate::path_guard::PathGuard::file(outer_deletes.clone());

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
                        suffix,
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
        // `derive-par-{creates,modifies,deletes}-{pid}{suffix}-*` on every
        // failed run. The names are recomputed from the same token, so a
        // concurrent run in this process keeps its own files.
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
                let (cp, mp, dp) = shard_xml_paths(scratch_dir, suffix, kind, shard_idx);
                drop(std::fs::remove_file(cp));
                drop(std::fs::remove_file(mp));
                drop(std::fs::remove_file(dp));
            }
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
        )?;

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
        host,
    )
    .map_err(map_emit_err)?;
    crate::debug::emit_marker("DERIVECHANGES_WRITE_END");

    // Clean up outer temp files. Explicit `commit()` first so the
    // guards know the happy path handled the cleanup; their Drop
    // would otherwise try to remove-already-removed files (harmless
    // but this is cleaner and silences the unused-variable warning).
    drop(outer_creates_guard.commit());
    drop(outer_modifies_guard.commit());
    drop(outer_deletes_guard.commit());
    drop(std::fs::remove_file(&outer_creates));
    drop(std::fs::remove_file(&outer_modifies));
    drop(std::fs::remove_file(&outer_deletes));

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
) -> Result<()> {
    let mut first_err = None;
    for shard_out in outputs {
        if first_err.is_some() {
            drop(std::fs::remove_file(&shard_out.creates_path));
            drop(std::fs::remove_file(&shard_out.modifies_path));
            drop(std::fs::remove_file(&shard_out.deletes_path));
            continue;
        }
        // `and` takes its argument eagerly, so all three calls run and each
        // removes its own source file even when an earlier copy failed; the
        // first error is the one kept.
        let appended = append_and_cleanup(creates_w, &shard_out.creates_path)
            .and(append_and_cleanup(modifies_w, &shard_out.modifies_path))
            .and(append_and_cleanup(deletes_w, &shard_out.deletes_path));
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
fn append_and_cleanup(out: &mut BufWriter<File>, src: &Path) -> Result<()> {
    let copied = File::open(src).and_then(|mut f| io::copy(&mut f, out));
    drop(std::fs::remove_file(src));
    copied
        .map(drop)
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))
}
