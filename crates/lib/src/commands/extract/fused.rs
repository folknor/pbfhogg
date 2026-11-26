//! Sorted simple extract's fused kind pass: one read, one decompression and
//! one decode per blob, selecting and framing on the worker.
//!
//! The kind-wide selection rule is by ID: an occurrence is written when any
//! scheduled occurrence of its kind and ID matches (the classify-then-write
//! form computes the whole kind's ID set first, then writes by it). A worker
//! can compute that rule only for IDs no blob outside its task holds.
//! [`fusable`] plans that before the kind starts: every blob of the kind
//! carries an indexdata ID range, and blobs whose ranges overlap form a
//! component, one task, so an ID's occurrences all lie in one task. On
//! ordinary input every component is one blob; on a history file, where an
//! ID's versions straddle a blob cut, the two blobs share a component. The
//! worker checks every claim it relies on exactly at decode
//! ([`IdTally::refuse_inexact`]; ADR-0013, a run that relies on a claim
//! checks it when it decodes), so the plan does not rest on a false range.
//! Within a one-blob component, a strictly increasing walk (canonical ID
//! order is injective) proves each ID occurs once and the per-occurrence
//! decision is the ID's; any equal or descending step switches the blob to
//! a blob-local ID set built from the selected occurrences, which is the
//! same OR the kind-wide set computed. A larger component always selects by
//! the union of its blobs' selected IDs. A kind that fails the plan (an
//! unindexed blob, a component that is not a contiguous schedule run or is
//! larger than `EXTRACT_FUSED_COMPONENT_MAX`) runs the classify-then-write
//! form instead.
//!
//! Each blob is walked twice on its worker, a selection walk and an emission
//! walk over the same decoded block, so nothing is framed before the claim
//! check and the decisions are complete: a component selects every blob
//! before it frames any. Only frames, the selected IDs and stats cross to
//! the ordered consumer. Error precedence inside a component follows the
//! two walks: a later blob's selection error wins over an earlier blob's
//! emission error.

use std::ops::Range;
use std::path::Path;

use crate::blob_meta::IdTally;
use crate::block_builder::{BlockBuilder, MemberData};
use crate::cat::CleanAttrs;
use crate::idset::IdSet;
use crate::read::input::PinnedInput;
use crate::writer::PbfWriter;
use crate::{Element, ElementKind, PrimitiveBlock};

use super::super::{
    BlockSink, FramedBlocks, Result, clean_metadata, clean_partial_metadata,
    ensure_node_capacity_local, ensure_relation_capacity_local, ensure_way_capacity_local,
    flush_local,
};
use super::common::{
    BboxInt, BlobDesc, Passthrough, dense_coords, dense_only_payload, merge_extract_stats,
    plain_coords, relation_has_matched_member, warn_passthrough_demotions,
};
use super::{ExtractStats, Region};
use crate::owned::{dense_node_metadata, element_metadata};

/// The overlap components of a kind schedule ([`components`]), or `None`
/// when the kind must classify, then write (module doc).
pub(super) fn fusable(schedule: &[&BlobDesc]) -> Option<Vec<Range<usize>>> {
    components(schedule.iter().map(|d| d.index))
}

/// The schedule's overlap components as contiguous position runs, in
/// schedule order, or `None` when the kind cannot fuse: a claim is missing
/// or inverted, a component's members are not one contiguous run of the
/// schedule (the ordered consumer writes a component's blobs together), or
/// a component holds more than `EXTRACT_FUSED_COMPONENT_MAX` blobs.
///
/// Two blobs that share an ID both claim it, so their closed numeric
/// intervals overlap and they land in one component: a component's union
/// of selections is the kind-wide ID rule for every ID it holds. The
/// intervals are compared numerically, never in canonical order, because
/// the claims are numeric extrema; disjoint claims always give singletons,
/// whatever their signs or order. Multi-extract's fused pass
/// (`multi_fused.rs`) uses the same planner.
pub(super) fn components(
    claims: impl ExactSizeIterator<Item = Option<crate::blob_meta::BlobIndex>>,
) -> Option<Vec<Range<usize>>> {
    let mut ranges: Vec<(i64, i64, usize)> = Vec::with_capacity(claims.len());
    for (position, index) in claims.enumerate() {
        match index {
            Some(idx) if idx.min_id <= idx.max_id => {
                ranges.push((idx.min_id, idx.max_id, position));
            }
            _ => return None,
        }
    }
    ranges.sort_unstable();
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut i = 0;
    while i < ranges.len() {
        let (_, mut reach, first) = ranges[i];
        let (mut lo, mut hi, mut count) = (first, first, 1usize);
        let mut j = i + 1;
        while j < ranges.len() && ranges[j].0 <= reach {
            reach = reach.max(ranges[j].1);
            lo = lo.min(ranges[j].2);
            hi = hi.max(ranges[j].2);
            count += 1;
            j += 1;
        }
        if hi - lo + 1 != count || count > crate::tunables::EXTRACT_FUSED_COMPONENT_MAX {
            return None;
        }
        out.push(lo..hi + 1);
        i = j;
    }
    out.sort_unstable_by_key(|r| r.start);
    Some(out)
}

/// The ordered pool window for a kind whose largest component is `largest`
/// blobs: the plain window divided by it, so the blobs admitted at once
/// never exceed the plain window's.
pub(super) fn component_window(decode_threads: usize, largest: usize) -> std::num::NonZeroUsize {
    let plain = crate::ordered_pool::default_window(decode_threads).get();
    std::num::NonZeroUsize::new((plain / largest.max(1)).max(1))
        .unwrap_or(std::num::NonZeroUsize::MIN)
}

/// What decides an occurrence of the pass's kind.
pub(super) enum Select<'a> {
    /// A node inside the region, on its exact decimicrodegree coordinates.
    Nodes {
        region: &'a Region,
        exact: &'a BboxInt,
    },
    /// A way with a ref among the region's nodes, written under `coords`.
    Ways {
        nodes: &'a IdSet,
        coords: crate::owned::WayCoords,
    },
    /// A relation with a matchable member selected (no relation closure).
    Relations { nodes: &'a IdSet, ways: &'a IdSet },
}

impl Select<'_> {
    fn kind(&self) -> ElementKind {
        match self {
            Select::Nodes { .. } => ElementKind::Node,
            Select::Ways { .. } => ElementKind::Way,
            Select::Relations { .. } => ElementKind::Relation,
        }
    }

    /// Whether the consumer needs the selected IDs (a later kind reads them).
    fn collects(&self) -> bool {
        !matches!(self, Select::Relations { .. })
    }

    /// `(id, selected)` for an element of the pass's kind, `None` otherwise.
    #[inline]
    fn decide(&self, element: &Element<'_>) -> crate::error::Result<Option<(i64, bool)>> {
        Ok(match (self, element) {
            (Select::Nodes { region, exact }, Element::DenseNode(dn)) => Some((
                dn.id(),
                region.contains_node(exact, dn.decimicro_lat().ok(), dn.decimicro_lon().ok())?,
            )),
            (Select::Nodes { region, exact }, Element::Node(n)) => Some((
                n.id(),
                region.contains_node(exact, n.decimicro_lat().ok(), n.decimicro_lon().ok())?,
            )),
            (Select::Ways { nodes, .. }, Element::Way(w)) => {
                Some((w.id(), w.refs().any(|r| nodes.get(r))))
            }
            (Select::Relations { nodes, ways }, Element::Relation(r)) => {
                Some((r.id(), relation_has_matched_member(r, nodes, ways)))
            }
            _ => None,
        })
    }
}

/// One fused blob's result.
enum Fused {
    /// Re-encoded frames, in order.
    Frames {
        frames: Vec<Vec<u8>>,
        stats: ExtractStats,
        ids: Vec<i64>,
        demoted: bool,
    },
    /// A verified passthrough candidate: its input frame, copied raw.
    Raw {
        frame: Vec<u8>,
        nodes: u64,
        ids: Vec<i64>,
    },
}

/// One worker's thread-local state.
struct Local {
    read_buf: crate::read::read_buf::ReadBuf,
    bb: BlockBuilder,
    pool: std::sync::Arc<crate::blob::DecompressPool>,
    st_scratch: Vec<(u32, u32)>,
    gr_scratch: Vec<(u32, u32)>,
    decisions: Vec<bool>,
    tuples: Vec<crate::scan::node::NodeTuple>,
    group_starts: Vec<(usize, usize)>,
    validate: ValidateScratch,
}

/// String-table and group-range scratch for a candidate's validation.
type ValidateScratch = (Vec<(u32, u32)>, Vec<(u32, u32)>);

/// Run one kind's fused pass over `schedule` in the `components` [`fusable`]
/// planned, writing in schedule order and inserting the selected IDs into
/// `selected` (left untouched for relations).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn fused_pass(
    input: &PinnedInput,
    schedule: &[&BlobDesc],
    components: &[Range<usize>],
    select: &Select<'_>,
    clean: &CleanAttrs,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut ExtractStats,
    selected: &mut IdSet,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    if schedule.is_empty() {
        return Ok(());
    }
    let shared_file = input.open()?;
    let shared_file = &shared_file;
    let source: std::sync::Arc<Path> = std::sync::Arc::from(input.path());
    let source = &source;
    let decode_threads = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    );
    let compression = writer.compression();
    let metrics = &host.invocation.metrics().writer;
    let mut consumer = Consumer {
        writer,
        stats,
        selected,
        verified: 0,
        demoted: 0,
    };
    let largest = components
        .iter()
        .map(ExactSizeIterator::len)
        .max()
        .unwrap_or(1);
    let cancel = &host.invocation;
    let mut workers = vec![(); decode_threads];
    crate::ordered_pool::run_ordered(
        components.len(),
        &mut workers,
        crate::ordered_pool::PoolConfig {
            window: component_window(decode_threads, largest),
            cancel: &host.invocation,
            waits: crate::ordered_pool::Waits::default(),
        },
        |_| Local {
            read_buf: crate::read::read_buf::ReadBuf::default(),
            bb: BlockBuilder::new(),
            pool: crate::blob::DecompressPool::new(&host.invocation),
            st_scratch: Vec::new(),
            gr_scratch: Vec::new(),
            decisions: Vec::new(),
            tuples: Vec::new(),
            group_starts: Vec::new(),
            validate: Default::default(),
        },
        |(), local: &mut Local, task| {
            let blobs = &schedule[components[task].clone()];
            let locate = |d: &BlobDesc, e: crate::error::Error| {
                e.locate(crate::error::Location::new(
                    Some(source),
                    None,
                    Some(crate::ByteOffset(d.frame_offset)),
                ))
            };
            let mut prepared = Vec::with_capacity(blobs.len());
            for d in blobs {
                if !prepared.is_empty() {
                    cancel.check()?;
                }
                let blob = prepare(shared_file, d, select, local).map_err(|e| locate(d, e));
                local.read_buf.release_outlier(&host.invocation);
                prepared.push(blob?);
            }
            // A larger component selects by the union of its blobs' IDs.
            let union: Option<Vec<i64>> = (prepared.len() > 1).then(|| {
                let mut all: Vec<i64> = prepared
                    .iter()
                    .flat_map(|p| p.ids().iter().copied())
                    .collect();
                all.sort_unstable();
                all.dedup();
                all
            });
            let mut out = Vec::with_capacity(prepared.len());
            for (d, p) in blobs.iter().zip(prepared) {
                out.push(
                    finish(
                        p,
                        union.as_deref(),
                        select,
                        clean,
                        compression,
                        metrics,
                        local,
                    )
                    .map_err(|e| locate(d, e))?,
                );
            }
            Ok(out)
        },
        &mut consumer,
    )?;
    let (verified, demoted) = (consumer.verified, consumer.demoted);
    if matches!(select, Select::Nodes { .. }) {
        warn_passthrough_demotions(verified, demoted, host);
    }
    Ok(())
}

/// One blob after its selection walk, before emission.
enum Prepared {
    /// A verified passthrough candidate: every occurrence selected by its
    /// own coordinates, so a union adds nothing to it.
    Raw {
        frame: Vec<u8>,
        nodes: u64,
        ids: Vec<i64>,
    },
    /// A decoded block, its per-occurrence decisions, its selected IDs, and
    /// whether its IDs strictly increase.
    Block {
        block: PrimitiveBlock,
        decisions: Vec<bool>,
        ids: Vec<i64>,
        unique: bool,
        demoted: bool,
    },
}

impl Prepared {
    fn ids(&self) -> &[i64] {
        match self {
            Prepared::Raw { ids, .. } | Prepared::Block { ids, .. } => ids,
        }
    }
}

/// Emit one prepared blob: by its own decisions when `union` is `None`
/// (a one-blob component), else by the component's sorted ID union.
fn finish(
    prepared: Prepared,
    union: Option<&[i64]>,
    select: &Select<'_>,
    clean: &CleanAttrs,
    compression: crate::write::compression::Compression,
    metrics: &crate::write::metrics::WriterMetrics,
    local: &mut Local,
) -> crate::error::Result<Fused> {
    let (block, decisions, mut ids, unique, demoted) = match prepared {
        Prepared::Raw { frame, nodes, ids } => return Ok(Fused::Raw { frame, nodes, ids }),
        Prepared::Block {
            block,
            decisions,
            ids,
            unique,
            demoted,
        } => (block, decisions, ids, unique, demoted),
    };
    let mut output = FramedBlocks::new(compression, metrics).cutting();
    let kind = select.kind();
    let coords = match select {
        Select::Ways { coords, .. } => *coords,
        Select::Nodes { .. } | Select::Relations { .. } => crate::owned::WayCoords::Drop,
    };
    let block_stats = match union {
        Some(set) => emit(
            &block,
            kind,
            clean,
            coords,
            |id| set.binary_search(&id).is_ok(),
            &mut local.bb,
            &mut output,
        )?,
        None if unique => {
            let mut next = decisions.iter().copied();
            emit(
                &block,
                kind,
                clean,
                coords,
                |_| next.next().unwrap_or(false),
                &mut local.bb,
                &mut output,
            )?
        }
        None => {
            ids.sort_unstable();
            ids.dedup();
            let set = &ids;
            emit(
                &block,
                kind,
                clean,
                coords,
                |id| set.binary_search(&id).is_ok(),
                &mut local.bb,
                &mut output,
            )?
        }
    };
    flush_local(&mut local.bb, &mut output)?;
    if decisions.capacity() > local.decisions.capacity() {
        local.decisions = decisions;
    }
    let frames = output.frames.into_iter().map(|(_, frame)| frame).collect();
    if !select.collects() {
        ids.clear();
    }
    Ok(Fused::Frames {
        frames,
        stats: block_stats,
        ids,
        demoted,
    })
}

/// Read, decompress, decode and select one blob (its selection walk and
/// exact claim check), or verify it for a raw copy.
#[allow(clippy::too_many_lines)]
fn prepare(
    file: &std::fs::File,
    d: &BlobDesc,
    select: &Select<'_>,
    local: &mut Local,
) -> crate::error::Result<Prepared> {
    let Some(claim) = d.index else {
        return Err(crate::error::Error::internal(
            "extract: an unindexed blob reached a fused kind pass",
        ));
    };
    let candidate = d.passthrough == Passthrough::Candidate;
    // A candidate reads its whole frame, so a verified one is copied from
    // the very bytes whose body was verified.
    let (read_at, read_len, body_at) = if candidate {
        let body_at = usize::try_from(d.offset - d.frame_offset)
            .map_err(|_| crate::error::Error::internal("extract: frame body offset"))?;
        (d.frame_offset, d.frame_size, body_at)
    } else {
        (d.offset, d.size, 0)
    };
    let raw = local.read_buf.read_exact_at(file, read_at, read_len)?;
    let body = raw
        .get(body_at..body_at + d.size)
        .ok_or_else(|| crate::error::Error::internal("extract: frame body range"))?;
    let mut buf = crate::blob::pool_get(Some(&local.pool), 0);
    crate::blob::decompress_blob_raw(body, &mut buf)?;

    let mut demoted = false;
    if let (true, Select::Nodes { region, exact }) = (candidate, select) {
        local.tuples.clear();
        let kinds = crate::scan::node::extract_node_tuples(
            &buf,
            &mut local.tuples,
            &mut local.group_starts,
        )?;
        kinds.refuse_under(d.kind)?;
        let mut tally = IdTally::default();
        let mut matching = 0usize;
        for t in &local.tuples {
            tally.add(t.id);
            if region.contains_decimicro(exact, t.lat, t.lon) {
                matching += 1;
            }
        }
        if matching == local.tuples.len() && dense_only_payload(&buf)? {
            // Under the dense-only shape the scan saw every element, so the
            // tally is the blob's and the claim is checked exactly.
            tally.refuse_inexact(&claim)?;
            let (st, groups) = &mut local.validate;
            crate::read::block::validate_in_place(&buf, st, groups, None, |_, _| {
                crate::read::block::NoProjection
            })?;
            let ids = local.tuples.iter().map(|t| t.id).collect();
            return Ok(Prepared::Raw {
                frame: raw.to_vec(),
                nodes: local.tuples.len() as u64,
                ids,
            });
        }
        demoted = true;
    }

    let block = PrimitiveBlock::from_vec_pooled_with_scratch(
        buf,
        &local.pool,
        &mut local.st_scratch,
        &mut local.gr_scratch,
    )?;
    block.refuse_foreign_kind(d.kind)?;

    // Selection walk: per-occurrence decisions, the exact claim tally, and
    // the strict-increase proof that each ID occurs once in the blob.
    let mut decisions = std::mem::take(&mut local.decisions);
    decisions.clear();
    let mut tally = IdTally::default();
    let mut previous: Option<crate::osm_id::OsmIdKey> = None;
    let mut unique = true;
    let mut ids: Vec<i64> = Vec::new();
    for element in block.elements_skip_metadata() {
        let Some((id, hit)) = select.decide(&element)? else {
            continue;
        };
        tally.add(id);
        let key = crate::osm_id::osm_id_key(id);
        if previous.is_some_and(|p| key <= p) {
            unique = false;
        }
        previous = Some(key);
        decisions.push(hit);
        if hit {
            ids.push(id);
        }
    }
    tally.refuse_inexact(&claim)?;
    Ok(Prepared::Block {
        block,
        decisions,
        ids,
        unique,
        demoted,
    })
}

/// The emission walk: every element of `kind`, in iteration order, written
/// when `want(id)` holds, through the same builder calls and cleaning as the
/// classify-then-write form's `extract_block_pass2`.
fn emit<O: BlockSink + ?Sized>(
    block: &PrimitiveBlock,
    kind: ElementKind,
    clean: &CleanAttrs,
    coords: crate::owned::WayCoords,
    mut want: impl FnMut(i64) -> bool,
    bb: &mut BlockBuilder,
    output: &mut O,
) -> crate::Result<ExtractStats> {
    let mut stats = ExtractStats {
        nodes_in_bbox: 0,
        nodes_from_ways: 0,
        nodes_from_relations: 0,
        ways_written: 0,
        ways_from_relations: 0,
        relations_written: 0,
        strategy: "",
    };
    let mut way = crate::owned::WayScratch::default();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    for element in block.elements() {
        match (&element, kind) {
            (Element::DenseNode(dn), ElementKind::Node) => {
                if want(dn.id()) {
                    ensure_node_capacity_local(bb, output)?;
                    let (lat, lon) = dense_coords(dn)?;
                    let meta = clean_metadata(dense_node_metadata(dn)?, clean);
                    bb.add_node_dense(dn.id(), lat, lon, dn.tags(), meta.as_ref());
                    stats.nodes_in_bbox += 1;
                }
            }
            (Element::Node(n), ElementKind::Node) => {
                if want(n.id()) {
                    ensure_node_capacity_local(bb, output)?;
                    let (lat, lon) = plain_coords(n)?;
                    let meta = clean_partial_metadata(
                        element_metadata(ElementKind::Node, n.id(), &n.info())?,
                        clean,
                    );
                    bb.add_node_partial(n.id(), lat, lon, n.tags(), meta.as_ref());
                    stats.nodes_in_bbox += 1;
                }
            }
            (Element::Way(w), ElementKind::Way) => {
                if want(w.id()) {
                    ensure_way_capacity_local(bb, output)?;
                    way.prepare(w, coords)?;
                    let meta = clean_partial_metadata(
                        element_metadata(ElementKind::Way, w.id(), &w.info())?,
                        clean,
                    );
                    way.write(bb, w.tags(), meta.as_ref());
                    stats.ways_written += 1;
                }
            }
            (Element::Relation(r), ElementKind::Relation) => {
                if want(r.id()) {
                    ensure_relation_capacity_local(bb, output)?;
                    r.members_into(&mut members_buf)?;
                    let meta = clean_partial_metadata(
                        element_metadata(ElementKind::Relation, r.id(), &r.info())?,
                        clean,
                    );
                    bb.add_relation_partial(r.id(), r.tags(), &members_buf, meta.as_ref());
                    stats.relations_written += 1;
                }
            }
            _ => {}
        }
    }
    Ok(stats)
}

/// The ordered consumer: frames written in schedule order, selected IDs
/// installed for the next kind.
struct Consumer<'a> {
    writer: &'a mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &'a mut ExtractStats,
    selected: &'a mut IdSet,
    verified: u64,
    demoted: u64,
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
        match item {
            Fused::Frames {
                frames,
                stats,
                ids,
                demoted,
            } => {
                merge_extract_stats(self.stats, &stats);
                self.demoted += u64::from(demoted);
                for id in ids {
                    self.selected.insert(id);
                }
                for frame in frames {
                    self.writer.write_raw_owned(frame)?;
                }
            }
            Fused::Raw { frame, nodes, ids } => {
                self.verified += 1;
                self.stats.nodes_in_bbox += nodes;
                for id in ids {
                    self.selected.insert(id);
                }
                self.writer.write_raw_owned(frame)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::components;
    use crate::ElementKind;
    use crate::blob_meta::BlobIndex;

    fn claim(min_id: i64, max_id: i64) -> Option<BlobIndex> {
        Some(BlobIndex {
            kind: ElementKind::Node,
            min_id,
            max_id,
            count: 1,
            bbox: None,
        })
    }

    fn plan(claims: &[Option<BlobIndex>]) -> Option<Vec<std::ops::Range<usize>>> {
        components(claims.iter().copied())
    }

    #[test]
    fn disjoint_claims_are_singletons_whatever_their_signs() {
        // Canonical order puts negatives after the positives.
        let claims = [claim(0, 0), claim(1, 5), claim(-1, -1), claim(-3, -2)];
        assert_eq!(plan(&claims), Some(vec![0..1, 1..2, 2..3, 3..4]));
    }

    #[test]
    fn a_shared_end_id_joins_neighbours() {
        let claims = [claim(1, 10), claim(10, 20), claim(21, 30), claim(30, 30)];
        assert_eq!(plan(&claims), Some(vec![0..2, 2..4]));
    }

    #[test]
    fn a_hole_an_absent_or_inverted_claim_or_an_oversized_component_refuses() {
        assert_eq!(plan(&[claim(1, 10), claim(11, 20), claim(5, 5)]), None);
        assert_eq!(plan(&[claim(1, 10), None]), None);
        assert_eq!(plan(&[claim(1, 10), claim(12, 11)]), None);
        let max = crate::tunables::EXTRACT_FUSED_COMPONENT_MAX;
        let chain = |n: usize| -> Vec<_> { (0i64..).take(n).map(|i| claim(i, i + 1)).collect() };
        assert_eq!(plan(&chain(max)), Some(std::iter::once(0..max).collect()));
        let over = chain(max + 1);
        assert_eq!(plan(&over), None);
    }
}
