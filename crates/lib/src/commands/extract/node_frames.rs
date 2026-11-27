//! Speculative node frames for single-region dependency extracts.

use std::sync::Mutex;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::block_builder::BlockBuilder;
use crate::cat::CleanAttrs;
use crate::read::block::{ClassificationBlock, SelectionBlock};
use crate::read::columnar::DenseNodeColumns;
use crate::read::input::InputFile;
use crate::scan::classify::{ScheduledBlob, parallel_project_phase};
use crate::tunables::{
    DEFAULT_ELEMENT_CAP, EXTRACT_NODE_FRAME_BYTES, EXTRACT_NODE_FRAME_HITS,
    EXTRACT_NODE_FRAME_RESERVATION, EXTRACT_NODE_FRAME_SCAN_QUANTUM, EXTRACT_NODE_FRAME_WORK,
};
use crate::write::compression::Compression;
use crate::write::metrics::WriterMetrics;
use crate::{Element, ElementKind, Result};

use super::Region;
use super::common::{
    BboxInt, BlobDesc, Extent, add_dense_node, add_plain_node, plain_coords, widen,
};

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub(super) hits: usize,
    pub(super) work: usize,
    pub(super) bytes: usize,
}

impl Limits {
    pub(super) const DEFAULT: Self = Self {
        hits: EXTRACT_NODE_FRAME_HITS,
        work: EXTRACT_NODE_FRAME_WORK,
        bytes: EXTRACT_NODE_FRAME_BYTES,
    };
}

pub(super) struct Options<'a> {
    pub(super) limits: Limits,
    pub(super) clean: &'a CleanAttrs,
    pub(super) compression: Compression,
}

pub(super) struct NodeFrame {
    pub(super) extent: (i64, i64),
    pub(super) nodes: u64,
    pub(super) bytes: Vec<u8>,
}

pub(super) type RetainedFrames = FxHashMap<u64, NodeFrame>;

struct Budget {
    work: usize,
    bytes: usize,
}

impl Budget {
    fn reserve(&mut self, work: usize) -> bool {
        if self.work < work || self.bytes < EXTRACT_NODE_FRAME_RESERVATION {
            return false;
        }
        self.work -= work;
        self.bytes -= EXTRACT_NODE_FRAME_RESERVATION;
        true
    }

    fn finish(&mut self, frame: Option<&NodeFrame>) {
        // A frame stays charged while the worker sends it, while it is
        // queued, and after the consumer receives it. Work is cumulative.
        self.bytes += EXTRACT_NODE_FRAME_RESERVATION - frame.map_or(0, |f| f.bytes.capacity());
    }
}

/// Collect the region's IDs and, on the indexed single-region route, retain
/// frames for isolated node blobs. Eligibility for final reuse also depends
/// on completion IDs, which are collected after this phase.
#[allow(clippy::too_many_arguments)]
pub(super) fn classify_nodes(
    input: &InputFile,
    schedule: &[ScheduledBlob],
    write_schedule: &[BlobDesc],
    positions: &[usize],
    region: &Region,
    bbox: &BboxInt,
    options: Option<Options<'_>>,
    selected: &mut crate::idset::IdSet,
    host: &crate::host::Entered<'_>,
) -> Result<Option<RetainedFrames>> {
    let mut extents = Vec::new();
    if options.is_some() {
        if schedule.len() != positions.len() {
            return Err(crate::Error::internal(
                "extract: node positions do not match the classify schedule",
            ));
        }
        extents = write_schedule
            .iter()
            .map(|d| {
                d.index
                    .filter(|i| i.kind == ElementKind::Node)
                    .map(|i| (i.min_id, i.max_id))
            })
            .collect();
    }
    let budget = options.as_ref().map(|o| {
        Mutex::new(Budget {
            work: o.limits.work,
            bytes: o.limits.bytes,
        })
    });
    let mut frames = RetainedFrames::default();
    let metrics = &host.invocation.metrics().writer;
    parallel_project_phase(
        input,
        schedule,
        None,
        || (DenseNodeColumns::new(), None),
        |seq, buffer, (columns, builder)| {
            let buffer = buffer.for_selection()?;
            let block = buffer.classify()?;
            let Some((options, budget)) = options.as_ref().zip(budget.as_ref()) else {
                let (hits, _) = node_hits::<false>(&block, columns, region, bbox)?;
                return Ok((hits, None, None));
            };
            let (hits, extent) = node_hits::<true>(&block, columns, region, bbox)?;
            let frame = extent.and_then(|(lo, hi)| {
                // The write schedule still trusts the claimed range. Keep
                // a frame only when the observed extent fits inside it, so
                // any of its local hits also selects that schedule entry.
                let claim = write_schedule[positions[seq]].index?;
                if lo < claim.min_id || hi > claim.max_id {
                    return None;
                }
                speculate(&buffer, &hits, (lo, hi), options, budget, builder, metrics)
            });
            Ok((hits, extent, frame))
        },
        |seq, (hits, extent, frame)| {
            for id in hits {
                selected.insert(id);
            }
            if options.is_some() {
                let pos = positions[seq];
                extents[pos] = extent;
                if let Some(frame) = frame {
                    frames.insert(write_schedule[pos].frame_offset, frame);
                }
            }
        },
        host,
    )?;
    Ok(options.map(|_| {
        host.invocation
            .counter("extract_node_frames_encoded", frames.len());
        retain_isolated(&extents, write_schedule, &mut frames);
        host.invocation
            .counter("extract_node_frames_admitted", frames.len());
        frames
    }))
}

fn node_hits<const OBSERVE: bool>(
    block: &ClassificationBlock<'_>,
    columns: &mut DenseNodeColumns,
    region: &Region,
    bbox: &BboxInt,
) -> Result<(Vec<i64>, Extent)> {
    let mut hits = Vec::new();
    let mut extent = None;
    if !matches!(region, Region::Bbox(_)) {
        for element in block.nodes() {
            let (id, lat, lon) = match &element {
                Element::DenseNode(n) => (n.id(), n.decimicro_lat().ok(), n.decimicro_lon().ok()),
                Element::Node(n) => (n.id(), n.decimicro_lat().ok(), n.decimicro_lon().ok()),
                _ => continue,
            };
            if OBSERVE {
                widen(&mut extent, id);
            }
            if region.contains_node(bbox, lat, lon)? {
                hits.push(id);
            }
        }
        return Ok((hits, extent));
    }

    block.decode_dense_columns(columns)?;
    columns.collect_matching_ids_bbox(
        bbox.min_lat,
        bbox.max_lat,
        bbox.min_lon,
        bbox.max_lon,
        &mut hits,
    );
    if OBSERVE {
        // Reduce the decoded ID column with no element or metadata walk.
        extent = columns.ids.split_first().map(|(&first, rest)| {
            rest.iter()
                .fold((first, first), |(lo, hi), &id| (lo.min(id), hi.max(id)))
        });
    }
    // The columns exclude plain Node messages.
    for node in block.plain_nodes() {
        let (lat, lon) = plain_coords(&node)?;
        if OBSERVE {
            widen(&mut extent, node.id());
        }
        if bbox.contains(lat, lon) {
            hits.push(node.id());
        }
    }
    Ok((hits, extent))
}

#[allow(clippy::too_many_arguments)]
fn speculate(
    block: &SelectionBlock,
    hits: &[i64],
    extent: (i64, i64),
    options: &Options<'_>,
    budget: &Mutex<Budget>,
    builder: &mut Option<BlockBuilder>,
    metrics: &WriterMetrics,
) -> Option<NodeFrame> {
    let hit_limit = options.limits.hits.min(DEFAULT_ELEMENT_CAP.get());
    if hits.is_empty() || hits.len() > hit_limit {
        return None;
    }
    let work = hits.len()
        + block
            .payload()
            .len()
            .div_ceil(EXTRACT_NODE_FRAME_SCAN_QUANTUM);
    let lock = || {
        budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    if !lock().reserve(work) {
        return None;
    }

    let bb = builder.get_or_insert_with(|| {
        BlockBuilder::with_element_cap_prealloc(DEFAULT_ELEMENT_CAP, hits.len())
    });
    let framed = frame_nodes(block, hits, hit_limit, options, budget, bb, metrics);
    // A speculative failure may be in a blob or record the ordinary write
    // pass never selects. Only that pass can decide whether to refuse it.
    if framed.is_err() {
        *builder = None;
    } else if !bb.is_empty() {
        bb.abandon_block();
    }
    let frame = framed.ok().flatten().map(|(bytes, nodes)| NodeFrame {
        extent,
        nodes,
        bytes,
    });
    lock().finish(frame.as_ref());
    frame
}

fn frame_nodes(
    block: &SelectionBlock,
    hits: &[i64],
    hit_limit: usize,
    options: &Options<'_>,
    budget: &Mutex<Budget>,
    bb: &mut BlockBuilder,
    metrics: &WriterMetrics,
) -> Result<Option<(Vec<u8>, u64)>> {
    let ids: FxHashSet<i64> = hits.iter().copied().collect();
    let mut nodes = 0;
    let mut fits = true;
    block.visit_retained(
        |kind, id| kind == ElementKind::Node && ids.contains(&id),
        |element| {
            if !fits || nodes == hit_limit || !bb.can_add_node() {
                fits = false;
                return Ok(());
            }
            if nodes >= hits.len() {
                // A version outside the region can still share a selected ID.
                let mut budget = budget
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if budget.work == 0 {
                    fits = false;
                    return Ok(());
                }
                budget.work -= 1;
            }
            match element {
                Element::DenseNode(n) => add_dense_node(bb, &n, options.clean)?,
                Element::Node(n) => add_plain_node(bb, &n, options.clean)?,
                _ => return Err(crate::Error::internal("extract: expected a selected node")),
            }
            nodes += 1;
            fits = bb.content_len() <= EXTRACT_NODE_FRAME_RESERVATION;
            Ok(())
        },
    )?;
    if !fits {
        return Ok(None);
    }
    let framed = bb.lend_encoded(|block| {
        use crate::read::blob_wire::{MAX_BLOB_HEADER_SIZE, MAX_BLOB_MESSAGE_SIZE};
        // Type, indexdata, datasize and tagdata have one-byte tags and at
        // most ten bytes per value or length varint. Leave oversize tag
        // indexes to the ordinary writer, which reports any it drops.
        let indexdata = block.index.serialize();
        let header = block
            .tagdata
            .map_or(0, <[u8]>::len)
            .saturating_add(indexdata.len() + b"OSMData".len() + 4 * (1 + 10));
        // Bound the full frame before compression and allocation. The
        // reservation is below the body limit, so a frame that fits here
        // never needs the ordinary writer's cuts.
        if block.bytes.len() as u64 > MAX_BLOB_MESSAGE_SIZE
            || header as u64 >= MAX_BLOB_HEADER_SIZE
            || crate::write::worst_case_body_len(block.bytes.len(), &options.compression)
                .saturating_add(header)
                .saturating_add(size_of::<u32>())
                > EXTRACT_NODE_FRAME_RESERVATION
        {
            return Ok(None);
        }
        let bytes = crate::writer::frame_blob_pipelined(
            block.bytes,
            &options.compression,
            Some(indexdata.as_slice()),
            block.tagdata,
            None,
            metrics,
        )?;
        if bytes.capacity() > EXTRACT_NODE_FRAME_RESERVATION {
            return Err(crate::Error::internal(
                "extract: node frame exceeded its reservation",
            ));
        }
        Ok(Some((bytes, nodes as u64)))
    })?;
    Ok(framed.flatten())
}

/// A sorted sweep detects intersection with any earlier range via the
/// running maximum end, and with any later range via the next start. The
/// input includes claimed ranges for node blobs skipped on spatial bounds.
fn retain_isolated(extents: &[Extent], schedule: &[BlobDesc], frames: &mut RetainedFrames) {
    if frames.is_empty() {
        return;
    }
    let mut spans = extents
        .iter()
        .enumerate()
        .filter_map(|(pos, e)| e.map(|(lo, hi)| (lo, hi, pos)))
        .collect::<Vec<_>>();
    if spans.iter().any(|&(lo, hi, _)| lo > hi) {
        frames.clear();
        return;
    }
    spans.sort_unstable_by_key(|&(lo, _, _)| lo);
    let mut prev_hi = None;
    for (i, &(lo, hi, pos)) in spans.iter().enumerate() {
        if prev_hi.is_some_and(|end| end >= lo)
            || spans.get(i + 1).is_some_and(|&(start, _, _)| start <= hi)
        {
            frames.remove(&schedule[pos].frame_offset);
        }
        prev_hi = Some(prev_hi.map_or(hi, |end: i64| end.max(hi)));
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use crate::MemberId;
    use crate::block_builder::{MemberData, Metadata, PartialMetadata};
    use crate::commands::HeaderOverrides;
    use crate::host::{Host, Invocation};
    use crate::output::{Output, Overwrite};
    use crate::read::input::PinnedInput;
    use crate::write::WriteOptions;
    use crate::write::header_builder::HeaderBuilder;
    use crate::writer::PbfWriter;

    use super::super::{ExtractStrategy, extract, extract_admitted, parse_bbox};
    use super::*;

    const OUTSIDE: i32 = 20_000_000;
    const SMALL: &str = "-1,-1,1,1";
    const LARGE: &str = "-5,-5,5,5";

    #[derive(Default)]
    struct Counters(Mutex<[i64; 3]>);

    impl crate::debug::InstrumentSink for Counters {
        fn marker(&self, _: &str) {}

        fn counter(&self, name: &str, value: i64) {
            let slot = match name {
                "extract_node_frames_encoded" => 0,
                "extract_node_frames_admitted" => 1,
                "extract_node_frames_reused" => 2,
                _ => return,
            };
            self.0.lock().expect("counter lock")[slot] = value;
        }
    }

    fn fixture(
        path: &Path,
        nodes: &[&[(i64, i32)]],
        ways: &[&[i64]],
        members: &[MemberData<'_>],
        indexed: bool,
    ) {
        let file = std::fs::File::create(path).expect("create fixture");
        let mut writer = PbfWriter::new(file, Compression::None);
        let header = HeaderBuilder::new().sorted().historical();
        writer
            .write_header(&header.build().expect("header"))
            .expect("write header");
        let mut bb = BlockBuilder::new();
        let mut write_block = |bb: &mut BlockBuilder| {
            let bytes = bb.take().expect("encode block").expect("nonempty block");
            if indexed {
                writer.write_primitive_block(bytes).expect("write block");
            } else {
                writer
                    .write_primitive_block_no_indexdata(bytes)
                    .expect("write unindexed block");
            }
        };
        for blob in nodes {
            for (i, &(id, lat)) in blob.iter().enumerate() {
                if i % 2 == 0 {
                    bb.add_node(
                        id,
                        lat,
                        0,
                        [("name", "dense")],
                        Some(&Metadata {
                            version: 1,
                            timestamp: 42,
                            changeset: 4,
                            uid: 2,
                            user: "user",
                            visible: true,
                        }),
                    );
                } else {
                    bb.add_node_partial(
                        id,
                        lat,
                        0,
                        [("name", "plain")],
                        Some(&PartialMetadata {
                            version: Some(2),
                            timestamp: Some(42),
                            ..PartialMetadata::default()
                        }),
                    );
                }
            }
            write_block(&mut bb);
        }
        for (i, refs) in ways.iter().enumerate() {
            bb.add_way(10 + i64::try_from(i).expect("way ID"), [], refs, None);
            write_block(&mut bb);
        }
        if !members.is_empty() {
            bb.add_relation(10, [("type", "multipolygon")], members, None);
            write_block(&mut bb);
        }
        writer.flush().expect("flush fixture");
    }

    #[derive(Clone, Copy)]
    enum Route {
        Public,
        NoFrames,
        Limited(Limits),
    }

    fn run(
        input: &Path,
        output: &Path,
        bbox: &str,
        strategy: ExtractStrategy,
        route: Route,
    ) -> (Vec<u8>, [u64; 6], [i64; 3]) {
        let counters = Arc::new(Counters::default());
        let owner = Host {
            invocation: Invocation::with_sink(Arc::clone(&counters) as _),
            ..Host::for_tests()
        };
        let output_slot =
            Output::new(output, Overwrite::Refuse, &owner.diagnostics).expect("stage output");
        let region = Region::Bbox(parse_bbox(bbox).expect("bbox"));
        let clean = CleanAttrs {
            uid: true,
            user: true,
            ..CleanAttrs::default()
        };
        let write = WriteOptions {
            force: true,
            ..WriteOptions::default()
        };
        let overrides = HeaderOverrides::default();
        let stats = match route {
            Route::Public => extract(
                input,
                output_slot,
                &region,
                strategy,
                true,
                &clean,
                &write,
                &overrides,
                &owner,
            )
            .expect("extract"),
            Route::NoFrames | Route::Limited(_) => {
                let host = owner.entered();
                let pin = PinnedInput::pin(input, &owner).expect("pin fixture");
                let bound = output_slot.bind(&owner.invocation).expect("bind output");
                let limits = match route {
                    Route::Limited(limits) => Some(limits),
                    _ => None,
                };
                let stats = extract_admitted(
                    &pin, &bound, &region, strategy, true, &clean, &write, &overrides, limits,
                    &host,
                )
                .expect("extract");
                bound.publish().expect("publish");
                stats
            }
        };
        let counts = [
            stats.nodes_in_bbox,
            stats.nodes_from_ways,
            stats.nodes_from_relations,
            stats.ways_written,
            stats.ways_from_relations,
            stats.relations_written,
        ];
        let frames = *counters.0.lock().expect("counter lock");
        (std::fs::read(output).expect("output bytes"), counts, frames)
    }

    fn parity(input: &Path, bbox: &str, route: Route, expected: [[i64; 3]; 2]) {
        let dir = crate::path_guard::tests::scratch();
        for (i, strategy) in [ExtractStrategy::CompleteWays, ExtractStrategy::Smart]
            .into_iter()
            .enumerate()
        {
            let (baseline, baseline_counts, _) = run(
                input,
                &dir.path().join(format!("baseline-{i}.pbf")),
                bbox,
                strategy,
                Route::NoFrames,
            );
            let (bytes, counts, frames) = run(
                input,
                &dir.path().join(format!("candidate-{i}.pbf")),
                bbox,
                strategy,
                route,
            );
            assert_eq!(frames, expected[i], "{strategy:?}");
            assert_eq!(counts, baseline_counts, "{strategy:?}");
            assert!(bytes == baseline, "{strategy:?} output bytes differ");
        }
    }

    #[test]
    fn small_extract_reuses_frames_including_outside_versions_of_local_hits() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        fixture(
            &input,
            &[&[(1, 0), (1, OUTSIDE), (2, OUTSIDE)], &[(3, 0), (4, 0)]],
            &[],
            &[],
            true,
        );
        parity(&input, SMALL, Route::Public, [[2, 2, 2]; 2]);
    }

    #[test]
    fn large_extract_exceeds_the_per_blob_hit_limit() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        let nodes: Vec<_> = (1..=i64::try_from(EXTRACT_NODE_FRAME_HITS).expect("hit limit") + 1)
            .map(|id| (id, OUTSIDE))
            .collect();
        fixture(&input, &[&nodes], &[], &[], true);
        parity(&input, LARGE, Route::Public, [[0, 0, 0]; 2]);
    }

    #[test]
    fn overlapping_observed_extents_require_rereads_even_without_duplicate_ids() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        // Canonical order places negative IDs by magnitude before positives.
        // The third range encloses the first two in signed numeric order.
        fixture(
            &input,
            &[
                &[(-1, 0), (-2, 0)],
                &[(-3, 0), (-4, 0)],
                &[(-5, 0), (1, 0)],
                &[(5, 0)],
            ],
            &[],
            &[],
            true,
        );
        parity(&input, SMALL, Route::Public, [[4, 1, 1]; 2]);
    }

    #[test]
    fn duplicate_ids_across_blobs_require_rereads_including_spatial_skips() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        for (i, lat) in [0, OUTSIDE].into_iter().enumerate() {
            let input = input.with_extension(format!("{i}.pbf"));
            fixture(
                &input,
                &[&[(1, 0), (3, 0)], &[(3, OUTSIDE), (4, lat)], &[(5, 0)]],
                &[],
                &[],
                true,
            );
            let encoded = if lat == 0 { 3 } else { 2 };
            parity(&input, SMALL, Route::Public, [[encoded, 1, 1]; 2]);
        }
    }

    #[test]
    fn way_and_smart_completion_inside_hit_blobs_require_rereads() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        fixture(
            &input,
            &[
                &[(1, 0), (2, OUTSIDE)],
                &[(3, 0), (4, OUTSIDE)],
                &[(5, 0), (6, OUTSIDE)],
                &[(7, 0), (8, OUTSIDE)],
            ],
            &[&[3, 4], &[6]],
            &[
                MemberData {
                    id: MemberId::Way(10),
                    role: "outer",
                },
                MemberData {
                    id: MemberId::Node(2),
                    role: "",
                },
                MemberData {
                    id: MemberId::Way(11),
                    role: "inner",
                },
            ],
            true,
        );
        parity(&input, SMALL, Route::Public, [[4, 4, 3], [4, 4, 1]]);
    }

    #[test]
    fn shared_work_and_byte_limits_each_force_rereads() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        fixture(&input, &[&[(1, 0)], &[(2, 0)]], &[], &[], true);
        for limit in [
            Limits {
                work: 2,
                ..Limits::DEFAULT
            },
            Limits {
                bytes: EXTRACT_NODE_FRAME_RESERVATION,
                ..Limits::DEFAULT
            },
        ] {
            parity(&input, SMALL, Route::Limited(limit), [[1, 1, 1]; 2]);
        }
    }

    #[test]
    fn unindexed_input_uses_the_original_write_pass() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        fixture(&input, &[&[(1, 0)], &[(2, 0)]], &[], &[], false);
        parity(&input, SMALL, Route::Public, [[0, 0, 0]; 2]);
    }
}
