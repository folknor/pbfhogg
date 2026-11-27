//! Repack's worker-side decode, encode and compression.

use std::num::NonZeroUsize;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use crate::block_builder::{BlockBuilder, MemberData};
use crate::commands::{
    FramedBlocks, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local,
};
use crate::owned::{dense_node_metadata, element_metadata};
use crate::write::metrics::WriterMetrics;
use crate::writer::Compression;
use crate::{Element, ElementKind, Error, PrimitiveBlock, Result};

pub(super) struct Config<'a, 'h> {
    pub cap: NonZeroUsize,
    pub prealloc_bound: usize,
    pub compression: Compression,
    pub preserve_locations: bool,
    pub host: &'a crate::host::Entered<'h>,
    pub input_path: &'a Arc<Path>,
}

impl Config<'_, '_> {
    pub fn frames(&self) -> FramedBlocks<'_> {
        let metrics: &WriterMetrics = &self.host.invocation.metrics().writer;
        FramedBlocks::new(self.compression, metrics)
    }

    pub fn worker(&self) -> WorkerScratch {
        WorkerScratch {
            builder: BlockBuilder::with_element_cap_prealloc(
                self.cap,
                self.prealloc_bound
                    .min(crate::tunables::DEFAULT_ELEMENT_CAP.get()),
            ),
            refs: Vec::new(),
            locations: Vec::new(),
        }
    }
}

type Frames = Vec<(ElementKind, Vec<u8>)>;

pub(super) struct Decoded {
    pub source: Option<PrimitiveBlock>,
    pub counts: [usize; 3],
    pub shrink: [Frames; 3],
}

pub(super) fn decode(
    block: PrimitiveBlock,
    worker: &mut WorkerScratch,
    config: &Config<'_, '_>,
) -> Result<Decoded> {
    let counts = kind_counts(&block)?;
    let mut shrink = std::array::from_fn(|_| Vec::new());
    let mut grow = false;
    for (kind, count) in super::KINDS.into_iter().zip(counts) {
        if count >= config.cap.get() {
            let mut frames = config.frames();
            worker.start_partition()?;
            worker.add_slice(
                &block,
                kind,
                0..count,
                config.preserve_locations,
                &mut frames,
            )?;
            worker.finish(&mut frames)?;
            shrink[usize::from(kind.ordinal())] = frames.frames;
        } else {
            grow |= count != 0;
        }
    }
    Ok(Decoded {
        source: grow.then_some(block),
        counts,
        shrink,
    })
}

/// A single-kind count is already available from validation. Mixed blocks
/// occur only on the unindexed schedule and need a count for each kind.
fn kind_counts(block: &PrimitiveBlock) -> Result<[usize; 3]> {
    let kinds = block.element_kinds();
    let mut counts = [0; 3];
    if let Some(kind) = kinds.single() {
        counts[usize::from(kind.ordinal())] = usize::try_from(block.element_count())
            .map_err(|_| Error::internal("repack element count exceeds usize"))?;
    } else if !kinds.is_empty() {
        for element in block.elements_skip_metadata() {
            counts[usize::from(element_kind(&element).ordinal())] += 1;
        }
    }
    Ok(counts)
}

fn element_kind(element: &Element<'_>) -> ElementKind {
    match element {
        Element::DenseNode(_) | Element::Node(_) => ElementKind::Node,
        Element::Way(_) => ElementKind::Way,
        Element::Relation(_) => ElementKind::Relation,
    }
}

pub(super) struct WorkerScratch {
    builder: BlockBuilder,
    refs: Vec<i64>,
    locations: Vec<(i32, i32)>,
}

impl WorkerScratch {
    pub fn start_partition(&mut self) -> Result<()> {
        if !self.builder.is_empty() {
            return Err(Error::internal(
                "repack worker started a partition with a nonempty builder",
            ));
        }
        Ok(())
    }

    pub fn finish(&mut self, frames: &mut FramedBlocks<'_>) -> Result<()> {
        flush_local(&mut self.builder, frames)?;
        self.release_scratch();
        Ok(())
    }

    pub fn release_scratch(&mut self) {
        crate::retention::release_scratch(&mut self.refs);
        crate::retention::release_scratch(&mut self.locations);
    }

    pub fn add_slice(
        &mut self,
        block: &PrimitiveBlock,
        kind: ElementKind,
        range: Range<usize>,
        preserve_locations: bool,
        frames: &mut FramedBlocks<'_>,
    ) -> Result<()> {
        let bb = &mut self.builder;
        let mut members_buf: Vec<MemberData<'_>> = Vec::new();
        let len = range.len();
        let elements = block
            .elements()
            .filter(|element| element_kind(element) == kind);
        let mut seen = 0;
        for element in elements.skip(range.start).take(len) {
            match element {
                Element::DenseNode(dn) => {
                    ensure_node_capacity_local(bb, frames)?;
                    let meta = dense_node_metadata(&dn)?;
                    bb.add_node_dense(
                        dn.id(),
                        dn.decimicro_lat()?,
                        dn.decimicro_lon()?,
                        dn.tags(),
                        meta.as_ref(),
                    );
                }
                Element::Node(node) => {
                    ensure_node_capacity_local(bb, frames)?;
                    let meta = element_metadata(kind, node.id(), &node.info())?;
                    bb.add_node_partial(
                        node.id(),
                        node.decimicro_lat()?,
                        node.decimicro_lon()?,
                        node.tags(),
                        meta.as_ref(),
                    );
                }
                Element::Way(way) => {
                    ensure_way_capacity_local(bb, frames)?;
                    encode_way(
                        bb,
                        &way,
                        &mut self.refs,
                        &mut self.locations,
                        preserve_locations,
                    )?;
                }
                Element::Relation(relation) => {
                    ensure_relation_capacity_local(bb, frames)?;
                    relation.members_into(&mut members_buf)?;
                    let meta = element_metadata(kind, relation.id(), &relation.info())?;
                    bb.add_relation_partial(
                        relation.id(),
                        relation.tags(),
                        &members_buf,
                        meta.as_ref(),
                    );
                }
            }
            seen += 1;
        }
        if seen != len {
            return Err(Error::internal(
                "repack slice exceeds its source's element count",
            ));
        }
        Ok(())
    }
}

/// Preserve the distinction between a way with inline coordinates and a
/// plain way, even under a LocationsOnWays header.
fn encode_way(
    bb: &mut BlockBuilder,
    way: &crate::Way<'_>,
    refs: &mut Vec<i64>,
    locations: &mut Vec<(i32, i32)>,
    preserve_locations: bool,
) -> Result<()> {
    refs.clear();
    refs.extend(way.refs());
    let meta = element_metadata(ElementKind::Way, way.id(), &way.info())?;
    if preserve_locations {
        locations.clear();
        for loc in way.node_locations() {
            locations.push((loc.decimicro_lat()?, loc.decimicro_lon()?));
        }
        crate::owned::check_way_locations(way.id(), refs.len(), locations.len(), true)?;
    }
    if preserve_locations && !locations.is_empty() {
        bb.add_way_with_locations_partial(way.id(), way.tags(), refs, locations, meta.as_ref());
    } else {
        bb.add_way_partial(way.id(), way.tags(), refs, meta.as_ref());
    }
    Ok(())
}
