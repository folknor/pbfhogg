//! Sorted simple multi-extract's fused kind pass: one read, one
//! decompression and one decode per blob for all N regions, selecting and
//! framing on the worker.
//!
//! The rule is single extract's (`fused.rs`), per region: an occurrence is
//! written to region `r` when any scheduled occurrence of its kind and ID
//! matched `r`. [`super::fused::components`] plans before the kind starts:
//! every blob of the kind carries an ID range, and blobs whose ranges
//! overlap (history files, where an ID's versions straddle a blob cut) form
//! one component, read as one task, so an ID's occurrences all lie in one
//! task. Each blob's claim is checked exactly at decode
//! (`IdTally::refuse_inexact`, ADR-0013 consequence 4) before anything of
//! its component leaves the worker. A kind the planner refuses runs the
//! classify-then-write form in `multi.rs`.
//!
//! A component of more than one blob selects every blob first, then builds
//! one `(id, region)` table from the blobs' admitted hits (a node blob's
//! hits are already thinned to the regions it is read for), emits each blob
//! through it intersected with that blob's own admission, and installs the
//! table once. Raw-copy eligibility stays each blob's own: a blob whose
//! every occurrence hits a candidate region by its own coordinates has every
//! ID selected there whatever the union adds.
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
//! A raw-copy candidate (nominated per region on its indexdata bbox, as in
//! the two-phase form) is read as its whole frame. It is copied raw to each
//! candidate region whose single extract would copy it: its payload is
//! DenseNodes alone, every occurrence's own spatial hits (before any union)
//! include the region, and its claim is exact. The other regions get the
//! blob's rebuilt blocks, so a region's output keeps the input's framing
//! exactly where its single extract does. The copy is the very bytes whose
//! body was validated.
//!
//! Error precedence differs from the two-phase form across blobs: an
//! emission error in an earlier blob now wins over a classification error
//! in a later one. Publication is all-or-nothing either way.

use crate::blob_meta::{BlobIndex, IdTally};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::read::columnar::DenseNodeColumns;
use crate::read::input::InputFile;
use crate::scan::classify::ScheduledBlob;
use crate::writer::PbfWriter;
use crate::{Element, ElementKind};

use super::super::{
    Result, clean_metadata, clean_partial_metadata, ensure_node_capacity_local,
    ensure_relation_capacity_local, ensure_way_capacity_local, flush_local,
};
use super::ExtractStats;
use super::common::{
    MatchableMember, dense_coords, dense_only_payload, matchable_members, plain_coords,
    warn_passthrough_demotions,
};
use super::membership::{BatchBuilder, Membership, PageBatch, admits, regions_of};
use super::multi::{RegionFraming, write_regions};
use super::polygon::RegionClassifier;
use crate::owned::{dense_node_metadata, element_metadata};

/// One blob of a fused kind: its schedule entry, its (exact-checked) claim,
/// the regions it is a raw-copy candidate for (`multi::raw_candidates`;
/// `None` for none), and for a node blob the regions it is read for
/// (`multi::node_blob_regions`; `None` for every region).
#[derive(Clone, Copy)]
pub(super) struct FusedBlob<'a> {
    pub(super) entry: ScheduledBlob,
    pub(super) claim: BlobIndex,
    pub(super) candidates: Option<&'a [u64]>,
    pub(super) admit: Option<&'a [u64]>,
}

/// A fused kind: its blobs in schedule order and the overlap components
/// (`fused::components`), each a contiguous run of `blobs` read as one task.
pub(super) struct FusedKind<'a> {
    pub(super) blobs: Vec<FusedBlob<'a>>,
    pub(super) components: Vec<std::ops::Range<usize>>,
}

/// What decides an occurrence of the pass's kind, per region.
pub(super) enum MultiSelect<'a> {
    /// Nodes inside each region on their exact decimicrodegree coordinates.
    /// Dense nodes can be tested on decoded columns when every region is a bbox.
    Nodes {
        classifier: &'a RegionClassifier<'a>,
        columnar: bool,
    },
    /// Ways: the OR of their refs' node masks, written under `coords`.
    Ways {
        nodes: &'a Membership,
        all: &'a [u64],
        coords: crate::owned::WayCoords,
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

/// One fused blob's result: the rebuilt regions' frames (`None` when every
/// region that reads the blob copies it raw), the verified input frame and
/// the regions it is copied raw to, and the blob's membership batch.
struct Fused {
    frames: Option<super::multi::RegionFrames>,
    raw: Option<RawPart>,
    batch: Option<PageBatch>,
    /// A candidate region the blob was not copied raw to.
    demoted: bool,
}

/// A verified candidate's input frame and the regions it is copied raw to,
/// each counted `nodes`.
struct RawPart {
    frame: Vec<u8>,
    mask: Vec<u64>,
    nodes: u64,
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
    /// A multi-blob component's sorted `(id, region)` union.
    union: Vec<(i64, u32)>,
    acc: Vec<u64>,
    regs: Vec<u32>,
    way: crate::owned::WayScratch,
    builder: BatchBuilder,
}

/// Run one kind's fused pass over `blobs` (which the gate admitted),
/// writing each region in schedule order and installing each blob's matches
/// into `members` when given (node and way membership; the caller freezes).
/// Returns nothing; node passes report passthrough demotions.
#[allow(clippy::too_many_arguments)]
pub(super) fn multi_fused_pass(
    input: &InputFile,
    kind: &FusedKind<'_>,
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
    let largest = kind
        .components
        .iter()
        .map(ExactSizeIterator::len)
        .max()
        .unwrap_or(1);
    let mut workers = vec![(); decode_threads];
    crate::ordered_pool::run_ordered(
        kind.components.len(),
        &mut workers,
        crate::ordered_pool::PoolConfig {
            window: super::fused::component_window(decode_threads, largest),
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
            union: Vec::new(),
            acc: vec![0u64; mw],
            regs: Vec::new(),
            way: crate::owned::WayScratch::default(),
            builder: BatchBuilder::new(n),
        },
        |(), local: &mut Local, task| {
            let blobs = &kind.blobs[kind.components[task].clone()];
            let mut prepared = Vec::with_capacity(blobs.len());
            for blob in blobs {
                if !prepared.is_empty() {
                    host.invocation.check()?;
                }
                let one = prepare(input, blob, select, n, local)
                    .map_err(|e| input.locate_blob(&blob.entry, e));
                local.read_buf.release_outlier(&host.invocation);
                prepared.push(one?);
            }
            // A larger component selects by the union of its blobs'
            // admitted hits.
            let union = prepared.len() > 1;
            if union {
                local.union.clear();
                for p in &prepared {
                    local.union.extend(p.pairs());
                }
                local.union.sort_unstable();
                local.union.dedup();
            }
            let mut out = Vec::with_capacity(prepared.len());
            for (i, (blob, p)) in blobs.iter().zip(prepared).enumerate() {
                out.push(
                    finish(
                        p,
                        blob,
                        union,
                        i == 0,
                        select,
                        n,
                        clean,
                        framing,
                        collect,
                        local,
                    )
                    .map_err(|e| input.locate_blob(&blob.entry, e))?,
                );
            }
            Ok(out)
        },
        &mut consumer,
    )?;
    if matches!(select, MultiSelect::Nodes { .. }) {
        warn_passthrough_demotions(consumer.verified, consumer.demoted, host);
    }
    Ok(())
}

/// Record the matching regions of one occurrence in ascending order.
#[inline]
#[allow(clippy::cast_possible_truncation)]
fn spatial_hits(
    classifier: &RegionClassifier<'_>,
    lat: i32,
    lon: i32,
    occ: u32,
    hits: &mut Vec<(u32, u32)>,
) {
    classifier.visit(lat, lon, None, |region| hits.push((occ, region as u32)));
}

/// Whether `ids` strictly increase in canonical OSM order.
fn strictly_increasing(ids: &[i64]) -> bool {
    ids.windows(2)
        .all(|w| crate::osm_id::osm_id_key(w[0]) < crate::osm_id::osm_id_key(w[1]))
}

/// Read, decompress, decode and select one blob: its selection walk, its
/// admission, its exact claim check and its raw-copy decision.
///
/// Occurrence indices and region numbers are `u32`: a validated block's
/// decompressed size bound keeps its element count far below `u32::MAX`, and
/// configured region counts fit in `u32`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::cognitive_complexity
)]
fn prepare(
    input: &InputFile,
    blob: &FusedBlob<'_>,
    select: &MultiSelect<'_>,
    n: usize,
    local: &mut Local,
) -> crate::error::Result<Prepared> {
    let entry = &blob.entry;
    // A candidate reads its whole frame, so a verified one is copied from
    // the very bytes whose body was validated.
    let candidates = blob
        .candidates
        .filter(|_| matches!(select, MultiSelect::Nodes { .. }));
    let candidate = candidates.is_some();
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
            classifier,
            columnar,
        } => {
            if *columnar {
                block.decode_dense_columns(&mut local.columns)?;
                let columns = &local.columns;
                for i in 0..columns.len() {
                    let (lat, lon) = (columns.lats[i], columns.lons[i]);
                    spatial_hits(classifier, lat, lon, i as u32, &mut local.dense_hits);
                }
                local
                    .dense_ids
                    .extend_from_slice(&columns.ids[..columns.len()]);
                for group in block.groups() {
                    for node in group.nodes() {
                        let (lat, lon) = plain_coords(&node)?;
                        let occ = local.plain_ids.len() as u32;
                        local.plain_ids.push(node.id());
                        spatial_hits(classifier, lat, lon, occ, &mut local.plain_hits);
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
                    spatial_hits(classifier, lat, lon, occ, hits);
                }
            }
        }
        MultiSelect::Ways { nodes, all, .. } => {
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
    // Each occurrence tests each region once, so a region with one hit per
    // occurrence holds every occurrence by its own coordinates: the single
    // extract's raw-copy rule, counted over occurrences, never over IDs.
    let raw_mask = match candidates {
        Some(candidates) if eligible && local.plain_ids.is_empty() => {
            let mut hits = vec![0usize; n];
            for &(_, region) in &local.dense_hits {
                hits[region as usize] += 1;
            }
            let mut mask = vec![0u64; candidates.len()];
            for region in regions_of(candidates) {
                if hits[region] == occurrences {
                    mask[region / 64] |= 1u64 << (region % 64);
                }
            }
            mask.iter().any(|&w| w != 0).then_some(mask)
        }
        _ => None,
    };
    let demoted = candidates.is_some_and(|candidates| match &raw_mask {
        Some(raw) => candidates.iter().zip(raw).any(|(c, r)| c & !r != 0),
        None => true,
    });
    let raw = match raw_mask {
        Some(mask) => Some(RawPart {
            frame: frame
                .take()
                .ok_or_else(|| crate::error::Error::internal("multi-extract: candidate frame"))?,
            mask,
            nodes: occurrences as u64,
        }),
        None => None,
    };
    Ok(Prepared {
        block,
        dense_ids: std::mem::take(&mut local.dense_ids),
        plain_ids: std::mem::take(&mut local.plain_ids),
        dense_hits: std::mem::take(&mut local.dense_hits),
        plain_hits: std::mem::take(&mut local.plain_hits),
        raw,
        demoted,
    })
}

/// One blob after its selection walk and claim check, before emission: its
/// block, its occurrence IDs and admitted hits (moved out of the worker's
/// scratch), and its raw-copy part.
struct Prepared {
    block: crate::PrimitiveBlock,
    dense_ids: Vec<i64>,
    plain_ids: Vec<i64>,
    dense_hits: Vec<(u32, u32)>,
    plain_hits: Vec<(u32, u32)>,
    raw: Option<RawPart>,
    demoted: bool,
}

impl Prepared {
    /// Every admitted `(id, region)` hit of the blob.
    fn pairs(&self) -> impl Iterator<Item = (i64, u32)> + '_ {
        let dense = self
            .dense_hits
            .iter()
            .map(|&(occ, region)| (self.dense_ids[occ as usize], region));
        let plain = self
            .plain_hits
            .iter()
            .map(|&(occ, region)| (self.plain_ids[occ as usize], region));
        dense.chain(plain)
    }
}

/// Emit one prepared blob. `union` is `None` for a one-blob component; for
/// a larger one it is the component's sorted `(id, region)` table in
/// `local.union`, built only from admitted hits, and emission reads it less
/// the regions this blob is not read for.
#[allow(
    clippy::cast_possible_truncation,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cognitive_complexity
)]
fn finish(
    prepared: Prepared,
    blob: &FusedBlob<'_>,
    union: bool,
    install: bool,
    select: &MultiSelect<'_>,
    n: usize,
    clean: &crate::cat::CleanAttrs,
    framing: &RegionFraming<'_>,
    collect: bool,
    local: &mut Local,
) -> crate::error::Result<Fused> {
    let Prepared {
        block,
        dense_ids,
        plain_ids,
        dense_hits,
        plain_hits,
        raw,
        demoted,
    } = prepared;
    local.dense_ids = dense_ids;
    local.plain_ids = plain_ids;
    local.dense_hits = dense_hits;
    local.plain_hits = plain_hits;
    // Every region the blob is read for copies it raw: no emission walk.
    if let Some(part) = &raw
        && (0..n).all(|r| !admits(blob.admit, r) || admits(Some(&part.mask), r))
    {
        let batch = if union {
            (collect && install).then(|| {
                for &(id, region) in &local.union {
                    local.builder.push(id, region as usize);
                }
                local.builder.finish()
            })
        } else {
            collect.then(|| {
                for &(occ, region) in &local.dense_hits {
                    local
                        .builder
                        .push(local.dense_ids[occ as usize], region as usize);
                }
                local.builder.finish()
            })
        };
        return Ok(Fused {
            frames: None,
            raw,
            batch,
            demoted,
        });
    }
    let skip = raw.as_ref().map(|part| part.mask.as_slice());

    let unique = !union
        && ((local.plain_ids.is_empty() && strictly_increasing(&local.dense_ids))
            || (local.dense_ids.is_empty() && strictly_increasing(&local.plain_ids)));
    if union {
        // The component's union, less the regions this blob is not read
        // for: its single extract of such a region never reads the blob.
        local.table.clear();
        let admit = blob.admit;
        local.table.extend(
            local
                .union
                .iter()
                .filter(|&&(_, region)| admits(admit, region as usize)),
        );
    } else if !unique {
        local.table.clear();
        for &(occ, region) in &local.dense_hits {
            local.table.push((local.dense_ids[occ as usize], region));
        }
        for &(occ, region) in &local.plain_hits {
            local.table.push((local.plain_ids[occ as usize], region));
        }
        local.table.sort_unstable();
        local.table.dedup();
    }
    // A component's table installs once, from its first blob: the union of
    // admitted hits is what each blob's own batch would install together.
    let batch = (collect && (!union || install)).then(|| {
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
            let table = if union { &local.union } else { &local.table };
            for &(id, region) in table {
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
    let coords = match select {
        MultiSelect::Ways { coords, .. } => *coords,
        MultiSelect::Nodes { .. } | MultiSelect::Relations { .. } => crate::owned::WayCoords::Drop,
    };
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    for element in block.elements() {
        match (&element, kind) {
            (Element::DenseNode(dn), ElementKind::Node) => {
                cursors.regions(true, dn.id(), unique, skip, local)?;
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
                cursors.regions(false, nd.id(), unique, skip, local)?;
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
                cursors.regions(true, w.id(), unique, None, local)?;
                if local.regs.is_empty() {
                    continue;
                }
                let wid = w.id();
                local.way.prepare(w, coords)?;
                let meta = clean_partial_metadata(
                    element_metadata(ElementKind::Way, wid, &w.info())?,
                    clean,
                );
                for &i in &local.regs {
                    let i = i as usize;
                    ensure_way_capacity_local(&mut local.bbs[i], &mut local.output[i])?;
                    local.way.write(&mut local.bbs[i], w.tags(), meta.as_ref());
                    counts[i] += 1;
                }
            }
            (Element::Relation(r), ElementKind::Relation) => {
                cursors.regions(true, r.id(), unique, None, local)?;
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
    Ok(Fused {
        frames: Some((frames, counts)),
        raw,
        batch,
        demoted,
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
    /// slice of the blob-local (or component) table, less the regions in
    /// `skip` (those the blob is copied raw to, so nothing of it is rebuilt
    /// or converted for them).
    #[inline]
    fn regions(
        &mut self,
        dense: bool,
        id: i64,
        unique: bool,
        skip: Option<&[u64]>,
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
        if let Some(skip) = skip {
            local
                .regs
                .retain(|&region| !admits(Some(skip), region as usize));
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

impl crate::ordered_pool::OrderedConsumer<Vec<Fused>> for Consumer<'_> {
    fn consume(&mut self, _task: usize, items: Vec<Fused>) -> crate::Result<()> {
        for item in items {
            self.consume_blob(item)?;
        }
        Ok(())
    }
}

impl Consumer<'_> {
    fn consume_blob(&mut self, item: Fused) -> crate::Result<()> {
        let Fused {
            frames,
            raw,
            batch,
            demoted,
        } = item;
        self.demoted += u64::from(demoted);
        if let (Some(members), Some(batch)) = (self.members.as_deref_mut(), batch) {
            members.install(batch);
        }
        if let Some(RawPart { frame, mask, nodes }) = raw {
            self.cancel.check()?;
            self.verified += 1;
            for region in regions_of(&mask) {
                self.writers[region].write_raw(&frame)?;
                *(self.stat_field)(&mut self.stats[region]) += nodes;
            }
        }
        match frames {
            Some(frames) => write_regions(frames, self.writers, self.stats, self.stat_field),
            None => Ok(()),
        }
    }
}
