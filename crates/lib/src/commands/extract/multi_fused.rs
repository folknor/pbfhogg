//! Sorted simple multi-extract's fused kind pass: one read, one
//! decompression and one decode per blob for all N regions, selecting and
//! framing on the worker.
//!
//! The rule is single extract's (`fused.rs`), per region: an occurrence is
//! written to region `r` when any scheduled occurrence of its kind and ID
//! matched `r`. [`super::fused::ranges_disjoint`] proves before the kind
//! starts that every blob of the kind carries an ID range and the ranges are
//! pairwise disjoint, so an ID's occurrences all lie in one blob, and each
//! blob's claim is checked exactly at decode (`IdTally::refuse_inexact`,
//! ADR-0013 consequence 4) before anything of it leaves the worker. A kind
//! that fails the gate runs the classify-then-write form in `multi.rs`.
//!
//! Each blob is walked twice on its worker. The selection walk records each
//! occurrence's regions as sparse hits, `(occurrence, region)` in occurrence
//! order, so the scratch is proportional to the matches rather than to
//! occurrences times the mask width. Dense and plain nodes keep separate
//! occurrence numbering: `elements()` yields a group's dense nodes in the
//! order `decode_dense_columns` appends them and its plain nodes in
//! `groups().nodes()` order. When the occurrences' IDs strictly increase in
//! canonical order (one list only), each occurrence's hits are its ID's
//! regions and the emission walk consumes them by cursor; otherwise the hits
//! become a blob-local sorted `(id, region)` table, the OR over every
//! occurrence of the ID, and emission looks each occurrence's ID up in it.
//!
//! A raw-copy candidate (nominated on its indexdata bbox, as in the two-phase
//! form) is read as its whole frame; it is copied raw to every region only
//! when its payload is DenseNodes alone, every occurrence's own spatial hits
//! (before any union) cover all N regions, and its claim is exact. The copy
//! is the very bytes whose body was validated.
//!
//! Error precedence differs from the two-phase form across blobs: an
//! emission error in an earlier blob now wins over a classification error
//! in a later one. Publication is all-or-nothing either way.

use crate::blob_meta::{BlobIndex, IdTally};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::read::columnar::DenseNodeColumns;
use crate::read::input::InputFile;
use crate::read::region_grid::RegionGrid;
use crate::scan::classify::ScheduledBlob;
use crate::writer::PbfWriter;
use crate::{Element, ElementKind};

use super::super::{
    Result, clean_metadata, clean_partial_metadata, ensure_node_capacity_local,
    ensure_relation_capacity_local, ensure_way_capacity_local, flush_local,
};
use super::common::{
    BboxInt, MatchableMember, dense_coords, dense_only_payload, matchable_members, plain_coords,
    warn_passthrough_demotions,
};
use super::membership::{BatchBuilder, Membership, PageBatch, admits, regions_of};
use super::multi::{RegionFraming, write_regions};
use super::{ExtractStats, Region};
use crate::owned::{dense_node_metadata, element_metadata};

/// One blob of a fused kind: its schedule entry, its (exact-checked) claim,
/// whether it is a raw-copy candidate, and for a node blob the regions it
/// is read for (`multi::node_blob_regions`; `None` for every region).
#[derive(Clone, Copy)]
pub(super) struct FusedBlob<'a> {
    pub(super) entry: ScheduledBlob,
    pub(super) claim: BlobIndex,
    pub(super) candidate: bool,
    pub(super) admit: Option<&'a [u64]>,
}

/// What decides an occurrence of the pass's kind, per region.
pub(super) enum MultiSelect<'a> {
    /// Nodes inside each region on their exact decimicrodegree coordinates.
    /// `bboxes` is set when every region is a bbox (dense nodes are then
    /// tested on decoded columns); `grid` prunes the regions tested.
    Nodes {
        regions: &'a [Region],
        inside: &'a [BboxInt],
        bboxes: Option<&'a [(i32, i32, i32, i32)]>,
        grid: Option<&'a RegionGrid>,
    },
    /// Ways: the OR of their refs' node masks.
    Ways {
        nodes: &'a Membership,
        all: &'a [u64],
    },
    /// Relations: the OR of their matchable members' masks.
    Relations {
        nodes: &'a Membership,
        ways: &'a Membership,
        all: &'a [u64],
    },
}

impl MultiSelect<'_> {
    fn kind(&self) -> ElementKind {
        match self {
            MultiSelect::Nodes { .. } => ElementKind::Node,
            MultiSelect::Ways { .. } => ElementKind::Way,
            MultiSelect::Relations { .. } => ElementKind::Relation,
        }
    }
}

/// One fused blob's result.
enum Fused {
    /// Each region's framed blocks and element count.
    Frames {
        frames: super::multi::RegionFrames,
        batch: Option<PageBatch>,
        demoted: bool,
    },
    /// A verified candidate's input frame, copied raw to every region.
    Raw {
        frame: Vec<u8>,
        nodes: u64,
        batch: Option<PageBatch>,
    },
}

/// One worker's thread-local state.
struct Local {
    read_buf: crate::read::read_buf::ReadBuf,
    pool: std::sync::Arc<crate::blob::DecompressPool>,
    st_scratch: Vec<(u32, u32)>,
    gr_scratch: Vec<(u32, u32)>,
    bbs: Vec<BlockBuilder>,
    output: Vec<Vec<OwnedBlock>>,
    columns: DenseNodeColumns,
    /// Dense-node (or, for ways and relations, every) occurrence IDs.
    dense_ids: Vec<i64>,
    plain_ids: Vec<i64>,
    dense_hits: Vec<(u32, u32)>,
    plain_hits: Vec<(u32, u32)>,
    table: Vec<(i64, u32)>,
    acc: Vec<u64>,
    regs: Vec<u32>,
    refs: Vec<i64>,
    builder: BatchBuilder,
}

/// Run one kind's fused pass over `blobs` (which the gate admitted),
/// writing each region in schedule order and installing each blob's matches
/// into `members` when given (node and way membership; the caller freezes).
/// Returns nothing; node passes report passthrough demotions.
#[allow(clippy::too_many_arguments)]
pub(super) fn multi_fused_pass(
    input: &InputFile,
    blobs: &[FusedBlob<'_>],
    select: &MultiSelect<'_>,
    n: usize,
    clean: &crate::cat::CleanAttrs,
    framing: &RegionFraming<'_>,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
    stat_field: fn(&mut ExtractStats) -> &mut u64,
    members: Option<&mut Membership>,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    let collect = members.is_some();
    let decode_threads = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    );
    let mw = super::membership::mask_words(n);
    let mut consumer = Consumer {
        writers,
        stats,
        stat_field,
        members,
        verified: 0,
        demoted: 0,
        cancel: &host.invocation,
    };
    let mut workers = vec![(); decode_threads];
    crate::ordered_pool::run_ordered(
        blobs.len(),
        &mut workers,
        crate::ordered_pool::PoolConfig {
            window: crate::ordered_pool::default_window(decode_threads),
            cancel: &host.invocation,
            waits: crate::ordered_pool::Waits::default(),
        },
        |_| Local {
            read_buf: crate::read::read_buf::ReadBuf::default(),
            pool: crate::blob::DecompressPool::new(&host.invocation),
            st_scratch: Vec::new(),
            gr_scratch: Vec::new(),
            bbs: (0..n).map(|_| BlockBuilder::new()).collect(),
            output: (0..n).map(|_| Vec::new()).collect(),
            columns: DenseNodeColumns::new(),
            dense_ids: Vec::new(),
            plain_ids: Vec::new(),
            dense_hits: Vec::new(),
            plain_hits: Vec::new(),
            table: Vec::new(),
            acc: vec![0u64; mw],
            regs: Vec::new(),
            refs: Vec::new(),
            builder: BatchBuilder::new(n),
        },
        |(), local: &mut Local, position| {
            let blob = &blobs[position];
            let result = fuse_blob(input, blob, select, n, clean, framing, collect, local)
                .map_err(|e| input.locate_blob(&blob.entry, e));
            local.read_buf.release_outlier(&host.invocation);
            result
        },
        &mut consumer,
    )?;
    if matches!(select, MultiSelect::Nodes { .. }) {
        warn_passthrough_demotions(consumer.verified, consumer.demoted, host);
    }
    Ok(())
}

/// Push every region `r` that `test(r)` accepts as a hit of occurrence
/// `occ`, ascending, through the grid when one is given. Region numbers are
/// `u32`, as the region grid numbers them.
#[inline]
#[allow(clippy::cast_possible_truncation)]
fn spatial_hits(
    grid: Option<&RegionGrid>,
    n: usize,
    lat: i32,
    lon: i32,
    occ: u32,
    hits: &mut Vec<(u32, u32)>,
    mut test: impl FnMut(usize) -> bool,
) {
    match grid {
        Some(grid) => {
            let start = hits.len();
            for &region in grid.candidates(lat, lon) {
                if test(region as usize) {
                    hits.push((occ, region));
                }
            }
            // Ascending regions per occurrence keep the per-region write
            // order of the two-phase form.
            hits[start..].sort_unstable();
        }
        None => {
            for region in 0..n {
                if test(region) {
                    hits.push((occ, region as u32));
                }
            }
        }
    }
}

/// Whether `ids` strictly increase in canonical OSM order.
fn strictly_increasing(ids: &[i64]) -> bool {
    ids.windows(2)
        .all(|w| crate::osm_id::osm_id_key(w[0]) < crate::osm_id::osm_id_key(w[1]))
}

/// Read, decompress, decode, select and frame one blob.
///
/// Occurrence indices and region numbers are `u32`: a validated block's
/// decompressed size bound keeps its element count far below `u32::MAX`, and
/// the region grid already numbers regions as `u32`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cognitive_complexity
)]
fn fuse_blob(
    input: &InputFile,
    blob: &FusedBlob<'_>,
    select: &MultiSelect<'_>,
    n: usize,
    clean: &crate::cat::CleanAttrs,
    framing: &RegionFraming<'_>,
    collect: bool,
    local: &mut Local,
) -> crate::error::Result<Fused> {
    let entry = &blob.entry;
    // A candidate reads its whole frame, so a verified one is copied from
    // the very bytes whose body was validated.
    let candidate = blob.candidate && matches!(select, MultiSelect::Nodes { .. });
    let mut frame: Option<Vec<u8>> = None;
    let mut buf = crate::blob::pool_get(Some(&local.pool), 0);
    if candidate {
        let raw = local.read_buf.read_exact_at(
            &**input.file(),
            entry.frame_start(),
            entry.frame_size(),
        )?;
        let body_at = usize::try_from(entry.data_offset() - entry.frame_start())
            .map_err(|_| crate::error::Error::internal("multi-extract: frame body offset"))?;
        let body = raw
            .get(body_at..body_at + entry.data_size())
            .ok_or_else(|| crate::error::Error::internal("multi-extract: frame body range"))?;
        crate::blob::decompress_blob_raw(body, &mut buf)?;
        frame = Some(raw.to_vec());
    } else {
        let raw = local.read_buf.read_exact_at(
            &**input.file(),
            entry.data_offset(),
            entry.data_size(),
        )?;
        crate::blob::decompress_blob_raw(raw, &mut buf)?;
    }
    let buffer = crate::read::block::BlockBuffer::new(
        buf,
        Some(&local.pool),
        &mut local.st_scratch,
        &mut local.gr_scratch,
        entry.claim(),
    );
    let (crate::read::block::NoProjection, validated) =
        buffer.project(|_, _| crate::read::block::NoProjection)?;
    let eligible = candidate && dense_only_payload(validated.bytes())?;
    let block = validated.into_block();

    // Selection walk.
    local.dense_ids.clear();
    local.plain_ids.clear();
    local.dense_hits.clear();
    local.plain_hits.clear();
    match select {
        MultiSelect::Nodes {
            regions,
            inside,
            bboxes,
            grid,
        } => {
            if let Some(bboxes) = bboxes {
                block.decode_dense_columns(&mut local.columns)?;
                let columns = &local.columns;
                for i in 0..columns.len() {
                    let (lat, lon) = (columns.lats[i], columns.lons[i]);
                    spatial_hits(*grid, n, lat, lon, i as u32, &mut local.dense_hits, |r| {
                        let (min_lat, max_lat, min_lon, max_lon) = bboxes[r];
                        lat >= min_lat && lat <= max_lat && lon >= min_lon && lon <= max_lon
                    });
                }
                local
                    .dense_ids
                    .extend_from_slice(&columns.ids[..columns.len()]);
                for group in block.groups() {
                    for node in group.nodes() {
                        let (lat, lon) = plain_coords(&node)?;
                        let occ = local.plain_ids.len() as u32;
                        local.plain_ids.push(node.id());
                        spatial_hits(*grid, n, lat, lon, occ, &mut local.plain_hits, |r| {
                            let (min_lat, max_lat, min_lon, max_lon) = bboxes[r];
                            lat >= min_lat && lat <= max_lat && lon >= min_lon && lon <= max_lon
                        });
                    }
                }
            } else {
                for element in block.elements_skip_metadata() {
                    let (id, lat, lon, dense) = match &element {
                        Element::DenseNode(node) => (
                            node.id(),
                            node.decimicro_lat().ok(),
                            node.decimicro_lon().ok(),
                            true,
                        ),
                        Element::Node(node) => (
                            node.id(),
                            node.decimicro_lat().ok(),
                            node.decimicro_lon().ok(),
                            false,
                        ),
                        _ => continue,
                    };
                    let (lat, lon) = super::common::checked_coords(lat, lon)?;
                    let (ids, hits) = if dense {
                        (&mut local.dense_ids, &mut local.dense_hits)
                    } else {
                        (&mut local.plain_ids, &mut local.plain_hits)
                    };
                    let occ = ids.len() as u32;
                    ids.push(id);
                    spatial_hits(*grid, n, lat, lon, occ, hits, |r| {
                        regions[r].contains_decimicro(&inside[r], lat, lon)
                    });
                }
            }
        }
        MultiSelect::Ways { nodes, all } => {
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element {
                    local.acc.fill(0);
                    for node_id in w.refs() {
                        if nodes.or_into(node_id, &mut local.acc) && local.acc == *all {
                            break;
                        }
                    }
                    let occ = local.dense_ids.len() as u32;
                    local.dense_ids.push(w.id());
                    for region in regions_of(&local.acc) {
                        local.dense_hits.push((occ, region as u32));
                    }
                }
            }
        }
        MultiSelect::Relations { nodes, ways, all } => {
            for element in block.elements_skip_metadata() {
                if let Element::Relation(r) = &element {
                    local.acc.fill(0);
                    for member in matchable_members(r) {
                        let hit = match member {
                            MatchableMember::Node(id) => nodes.or_into(id, &mut local.acc),
                            MatchableMember::Way(id) => ways.or_into(id, &mut local.acc),
                        };
                        if hit && local.acc == *all {
                            break;
                        }
                    }
                    let occ = local.dense_ids.len() as u32;
                    local.dense_ids.push(r.id());
                    for region in regions_of(&local.acc) {
                        local.dense_hits.push((occ, region as u32));
                    }
                }
            }
        }
    }

    // A node blob's occurrences hit only the regions it is read for, as in
    // each single extract (`multi::node_blob_regions`). A raw-copy
    // candidate is admitted by every region (its bbox lies inside each), so
    // this never thins a candidate's hits.
    if let Some(admit) = blob.admit {
        let admit = Some(admit);
        local
            .dense_hits
            .retain(|&(_, region)| admits(admit, region as usize));
        local
            .plain_hits
            .retain(|&(_, region)| admits(admit, region as usize));
    }

    // The exact claim check over every occurrence, before anything leaves.
    let mut tally = IdTally::default();
    for &id in local.dense_ids.iter().chain(&local.plain_ids) {
        tally.add(id);
    }
    tally.refuse_inexact(&blob.claim)?;

    let occurrences = local.dense_ids.len() + local.plain_ids.len();
    // Each occurrence tests each region once, so a hit total of N per
    // occurrence means every occurrence's own hits cover every region.
    if eligible && local.plain_ids.is_empty() && local.dense_hits.len() == n * occurrences {
        let batch = collect.then(|| {
            for &(occ, region) in &local.dense_hits {
                local
                    .builder
                    .push(local.dense_ids[occ as usize], region as usize);
            }
            local.builder.finish()
        });
        let frame =
            frame.ok_or_else(|| crate::error::Error::internal("multi-extract: candidate frame"))?;
        return Ok(Fused::Raw {
            frame,
            nodes: occurrences as u64,
            batch,
        });
    }

    let unique = (local.plain_ids.is_empty() && strictly_increasing(&local.dense_ids))
        || (local.dense_ids.is_empty() && strictly_increasing(&local.plain_ids));
    local.table.clear();
    if !unique {
        for &(occ, region) in &local.dense_hits {
            local.table.push((local.dense_ids[occ as usize], region));
        }
        for &(occ, region) in &local.plain_hits {
            local.table.push((local.plain_ids[occ as usize], region));
        }
        local.table.sort_unstable();
        local.table.dedup();
    }
    let batch = collect.then(|| {
        if unique {
            for &(occ, region) in &local.dense_hits {
                local
                    .builder
                    .push(local.dense_ids[occ as usize], region as usize);
            }
            for &(occ, region) in &local.plain_hits {
                local
                    .builder
                    .push(local.plain_ids[occ as usize], region as usize);
            }
        } else {
            for &(id, region) in &local.table {
                local.builder.push(id, region as usize);
            }
        }
        local.builder.finish()
    });

    // Emission walk.
    for v in &mut local.output {
        v.clear();
    }
    let mut counts = vec![0u64; n];
    let mut cursors = Cursors::default();
    let kind = select.kind();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    for element in block.elements() {
        match (&element, kind) {
            (Element::DenseNode(dn), ElementKind::Node) => {
                cursors.regions(true, dn.id(), unique, local)?;
                if local.regs.is_empty() {
                    continue;
                }
                let id = dn.id();
                let (lat, lon) = dense_coords(dn)?;
                let meta = clean_metadata(dense_node_metadata(dn)?, clean);
                for &i in &local.regs {
                    let i = i as usize;
                    ensure_node_capacity_local(&mut local.bbs[i], &mut local.output[i])?;
                    local.bbs[i].add_node_dense(id, lat, lon, dn.tags(), meta.as_ref());
                    counts[i] += 1;
                }
            }
            (Element::Node(nd), ElementKind::Node) => {
                cursors.regions(false, nd.id(), unique, local)?;
                if local.regs.is_empty() {
                    continue;
                }
                let id = nd.id();
                let (lat, lon) = plain_coords(nd)?;
                let meta = clean_partial_metadata(
                    element_metadata(ElementKind::Node, id, &nd.info())?,
                    clean,
                );
                for &i in &local.regs {
                    let i = i as usize;
                    ensure_node_capacity_local(&mut local.bbs[i], &mut local.output[i])?;
                    local.bbs[i].add_node_partial(id, lat, lon, nd.tags(), meta.as_ref());
                    counts[i] += 1;
                }
            }
            (Element::Way(w), ElementKind::Way) => {
                cursors.regions(true, w.id(), unique, local)?;
                if local.regs.is_empty() {
                    continue;
                }
                let wid = w.id();
                local.refs.clear();
                local.refs.extend(w.refs());
                let meta = clean_partial_metadata(
                    element_metadata(ElementKind::Way, wid, &w.info())?,
                    clean,
                );
                for &i in &local.regs {
                    let i = i as usize;
                    ensure_way_capacity_local(&mut local.bbs[i], &mut local.output[i])?;
                    local.bbs[i].add_way_partial(wid, w.tags(), &local.refs, meta.as_ref());
                    counts[i] += 1;
                }
            }
            (Element::Relation(r), ElementKind::Relation) => {
                cursors.regions(true, r.id(), unique, local)?;
                if local.regs.is_empty() {
                    continue;
                }
                let rid = r.id();
                r.members_into(&mut members_buf)?;
                let meta = clean_partial_metadata(
                    element_metadata(ElementKind::Relation, rid, &r.info())?,
                    clean,
                );
                for &i in &local.regs {
                    let i = i as usize;
                    ensure_relation_capacity_local(&mut local.bbs[i], &mut local.output[i])?;
                    local.bbs[i].add_relation_partial(rid, r.tags(), &members_buf, meta.as_ref());
                    counts[i] += 1;
                }
            }
            _ => {}
        }
    }
    cursors.finish(local)?;
    for i in 0..n {
        flush_local(&mut local.bbs[i], &mut local.output[i])?;
    }
    let mut frames = Vec::with_capacity(n);
    for (i, blocks) in local.output.iter_mut().enumerate() {
        frames.push(framing.frame_region(i, blocks.drain(..))?);
    }
    Ok(Fused::Frames {
        frames: (frames, counts),
        batch,
        demoted: candidate,
    })
}

/// The emission walk's position in each occurrence list and its hits.
#[derive(Default)]
struct Cursors {
    dense_occ: u32,
    plain_occ: u32,
    dense_hit: usize,
    plain_hit: usize,
}

impl Cursors {
    /// Fill `local.regs` with the next occurrence's regions (ascending) from
    /// the dense or plain list: its run of hits when `unique`, else its ID's
    /// slice of the blob-local table.
    #[inline]
    fn regions(
        &mut self,
        dense: bool,
        id: i64,
        unique: bool,
        local: &mut Local,
    ) -> crate::error::Result<()> {
        local.regs.clear();
        let (occ, at, hits) = if dense {
            (&mut self.dense_occ, &mut self.dense_hit, &local.dense_hits)
        } else {
            (&mut self.plain_occ, &mut self.plain_hit, &local.plain_hits)
        };
        let this = *occ;
        *occ += 1;
        if unique {
            while let Some(&(o, r)) = hits.get(*at) {
                if o > this {
                    break;
                }
                if o < this {
                    return Err(crate::error::Error::internal(
                        "multi-extract: a fused hit fell behind its occurrence",
                    ));
                }
                local.regs.push(r);
                *at += 1;
            }
        } else {
            let start = local.table.partition_point(|&(t, _)| t < id);
            for &(t, r) in &local.table[start..] {
                if t != id {
                    break;
                }
                local.regs.push(r);
            }
        }
        Ok(())
    }

    /// Refuse a hit no emission occurrence consumed.
    fn finish(&self, local: &Local) -> crate::error::Result<()> {
        let unconsumed =
            |occ: u32, hits: &[(u32, u32)]| hits.last().is_some_and(|&(o, _)| o >= occ);
        if unconsumed(self.dense_occ, &local.dense_hits)
            || unconsumed(self.plain_occ, &local.plain_hits)
        {
            return Err(crate::error::Error::internal(
                "multi-extract: the fused walks disagree on the blob's occurrences",
            ));
        }
        Ok(())
    }
}

/// The ordered consumer: membership pages installed and region frames (or a
/// verified raw frame) written in schedule order.
struct Consumer<'a> {
    writers: &'a mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &'a mut [ExtractStats],
    stat_field: fn(&mut ExtractStats) -> &mut u64,
    members: Option<&'a mut Membership>,
    verified: u64,
    demoted: u64,
    cancel: &'a crate::host::Recorder,
}

impl crate::ordered_pool::OrderedConsumer<Fused> for Consumer<'_> {
    fn consume(&mut self, _task: usize, item: Fused) -> crate::Result<()> {
        match item {
            Fused::Frames {
                frames,
                batch,
                demoted,
            } => {
                self.demoted += u64::from(demoted);
                if let (Some(members), Some(batch)) = (self.members.as_deref_mut(), batch) {
                    members.install(batch);
                }
                write_regions(frames, self.writers, self.stats, self.stat_field)
            }
            Fused::Raw {
                frame,
                nodes,
                batch,
            } => {
                self.cancel.check()?;
                self.verified += 1;
                if let (Some(members), Some(batch)) = (self.members.as_deref_mut(), batch) {
                    members.install(batch);
                }
                for (writer, stats) in self.writers.iter_mut().zip(self.stats.iter_mut()) {
                    writer.write_raw(&frame)?;
                    *(self.stat_field)(stats) += nodes;
                }
                Ok(())
            }
        }
    }
}
