//! Multi-extract: single-pass multi-region extraction for sorted inputs (simple strategy).

use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::writer::{Compression, PbfWriter};
use crate::{Element, ElementKind, PrimitiveBlock};

use super::super::{
    HeaderOverrides, Result, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local,
};
use crate::read::input::{InputFile, PinnedInput};
use crate::read::region_grid::RegionGrid;
use crate::scan::classify::ScheduledBlob;

use super::common::{
    BboxInt, MatchableMember, Passthrough, dense_only_payload, matchable_members,
    spatial_blob_filter, warn_passthrough_demotions,
};
use super::membership::{BatchBuilder, Membership, PageBatch, full_mask, mask_words, regions_of};
use super::{ExtractStats, Region};

use crate::owned::{dense_node_metadata, element_metadata, relation_members_into};

/// Try single-pass multi-extract: read the PBF once, classify each element
/// against all N regions, write to N output files. Returns `None` to fall
/// back to sequential if the input isn't sorted.
///
/// Simple strategy only (no per-region way/relation ID tracking beyond what
/// fits in memory). The pool workers compress and frame each region's blocks
/// (`RegionFraming`); the consumer copies the frames into N sync-mode
/// PbfWriters (no per-writer thread).
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cognitive_complexity
)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn try_extract_multi_single_pass(
    input: &PinnedInput,
    regions: &[Region],
    outputs: &crate::output::BoundOutputSet,
    set_bounds: bool,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<Option<Vec<ExtractStats>>> {
    use std::io::BufWriter;

    // Check if input is sorted.
    let header = {
        let mut br = crate::BlobReader::open_in(input, direct_io, host)?;
        match br.next() {
            Some(Ok(blob)) => match blob.decode()? {
                crate::blob::BlobDecode::OsmHeader(h) => {
                    if !h.is_sorted() {
                        return Ok(None); // fall back to sequential
                    }
                    h
                }
                _ => return Ok(None),
            },
            Some(Err(e)) => {
                // Declining here keeps the historical fallback (the
                // sequential passes then fail on their own read), but the
                // first error must not vanish: name it before falling back.
                crate::host::diag!(
                    host.diagnostics,
                    Note,
                    "extract.multi.probe_read_error",
                    "Multi-extract single-pass probe could not read the first blob: {}",
                    e.report()
                );
                host.invocation.counter("multi_extract_probe_read_error", 1);
                return Ok(None);
            }
            None => return Ok(None),
        }
    };

    let n = regions.len();
    host.invocation.marker("MULTI_EXTRACT_START");
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "extract.multi.single_pass",
        "[multi-extract] single-pass: {n} regions, simple strategy"
    );

    // Precompute per-region integer bboxes.
    let bbox_ints: Vec<BboxInt> = regions
        .iter()
        .map(|region| BboxInt::from_bbox(region.bbox()))
        .collect();
    let inside_bbox_ints: Vec<BboxInt> = regions
        .iter()
        .map(|region| BboxInt::inside_bbox(region.bbox()))
        .collect();

    // Union bbox for blob-level spatial skip.
    let union_bbox = BboxInt {
        min_lon: bbox_ints
            .iter()
            .map(|b| b.min_lon)
            .min()
            .unwrap_or(i32::MIN),
        min_lat: bbox_ints
            .iter()
            .map(|b| b.min_lat)
            .min()
            .unwrap_or(i32::MIN),
        max_lon: bbox_ints
            .iter()
            .map(|b| b.max_lon)
            .max()
            .unwrap_or(i32::MAX),
        max_lat: bbox_ints
            .iter()
            .map(|b| b.max_lat)
            .max()
            .unwrap_or(i32::MAX),
    };
    let spatial_filter = spatial_blob_filter(&union_bbox);

    // Open N sync-mode writers. Each owns a 256 KiB buffer; publication
    // syncs each staged output after the final flush. The writers only copy
    // frames: compression runs inside the ordered pool's tasks
    // (`RegionFraming`), so all N regions share the pool's one task window
    // and its ordinal order is each region's block order. Compressing on
    // the consumer thread made every write phase run at 2-3 cores with the
    // workers parked on result send (planet 5-region: 613 s of write phases,
    // 608 s of it compression). N pipelined writers would instead give each
    // region its own permits, delivery window and writer thread - N
    // buffering allowances for one stream of tasks.
    let mut writers: Vec<PbfWriter<BufWriter<std::fs::File>>> = Vec::with_capacity(n);
    for (region, output) in regions.iter().zip(outputs.iter()) {
        let bbox = region.bbox();
        let header_bytes = super::super::output_header::build_one(
            &header,
            &super::output_facts(set_bounds, bbox),
            overrides,
            host,
        )?
        .bytes;
        // The session and the writer under the run's one token.
        let mut w = output.sync_pbf_writer(compression)?;
        // The writer names its output in any error it returns.
        w.write_header(&header_bytes)?;
        writers.push(w);
    }
    let framing = RegionFraming {
        compression,
        destinations: outputs
            .iter()
            .map(crate::output::BoundOutput::destination)
            .collect(),
        invocation: &host.invocation,
    };

    // One shared membership per kind: each answers which regions hold an ID
    // with one lookup, and stores each member once with its region mask
    // rather than once per region (membership.rs has the representation and
    // its costs; N per-region IdSets were dense over the whole ID history at
    // planet, N bits per ID position). Regions may overlap; a mask records
    // every region an ID belongs to. Workers encode each blob's matches into
    // pages and the consumer only installs or merges them: direct hot-loop
    // set insertion and whole-worker dense accumulation have already lost on
    // time and memory, respectively (reference/performance-history.md). No
    // up-front refusal bounds memory: header ID ranges cannot predict
    // membership, and an admission budget needs a caller's memory policy, not
    // a machine-RAM guess inside this library.
    let mw = mask_words(n);
    let all_regions = full_mask(n);
    let mut node_members = Membership::new(n);
    let mut way_members = Membership::new(n);
    let mut relation_members = Membership::new(n);
    let mut stats: Vec<ExtractStats> = (0..n)
        .map(|_| ExtractStats {
            nodes_in_bbox: 0,
            nodes_from_ways: 0,
            nodes_from_relations: 0,
            ways_written: 0,
            ways_from_relations: 0,
            relations_written: 0,
            strategy: "simple",
        })
        .collect();

    // Build schedules by element type for parallel classification.
    // Walk via the pread-only HeaderWalker so blob bodies stay out of the
    // page cache during the scan - the per-kind classification passes open
    // fresh fds and pread only the blobs they need.
    //
    // This second open is deliberate too: the probe above is the sole judge
    // of the leading header (it decides the decline contract, honouring
    // direct I/O), and the walk wants its own FADV_RANDOM descriptor. The walk
    // starts at the beginning and consumes the leading frame without
    // re-judging it; the walker would accept a data blob there, so it is
    // not a second missing-header check.
    host.invocation.marker("MULTI_SCHEDULE_SCAN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, host)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input.path()))?;

    // Each node blob with its passthrough state, in one vector, so nothing
    // pairs a schedule with a parallel table by position.
    let mut node_blobs: Vec<NodeBlob> = Vec::new();
    let mut way_schedule: Vec<ScheduledBlob> = Vec::new();
    let mut relation_schedule: Vec<ScheduledBlob> = Vec::new();
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        walker.observe_filter_index(meta.index.as_ref());
        let entry = ScheduledBlob::from_meta(&meta, crate::scan::classify::KindClaims::Refuse)?;
        // Both decisions below are taken on indexdata claims, never decoded
        // to check: a blob claimed as nodes whose bbox misses every region
        // is skipped unread, and each blob joins the one per-kind schedule
        // its kind names, so a mislabelled blob's elements are missed or
        // dropped. Deliberate: claims are believed, not verified
        // (reference/indexdata-trust.md); `check --index` is the verifier.
        if let Some(idx) = meta.index.as_ref() {
            if !spatial_filter.wants_index(idx) {
                continue;
            }
            match idx.kind {
                crate::ElementKind::Node => {
                    // Raw passthrough is only sound for bbox regions - polygon
                    // regions can exclude nodes inside the bbox but outside the
                    // polygon boundary or inside holes - and only when the
                    // blob goes to every region. This nominates on the
                    // indexdata bbox claim; the node classify phase verifies.
                    let candidate = idx.bbox.as_ref().is_some_and(|blob_bbox| {
                        inside_bbox_ints
                            .iter()
                            .zip(regions.iter())
                            .all(|(bi, region)| {
                                matches!(region, Region::Bbox(_))
                                    && crate::BlobBbox::new(
                                        bi.min_lat, bi.max_lat, bi.min_lon, bi.max_lon,
                                    )
                                    .contains(blob_bbox)
                            })
                    });
                    node_blobs.push(NodeBlob {
                        entry,
                        passthrough: if candidate {
                            Passthrough::Candidate
                        } else {
                            Passthrough::No
                        },
                    });
                }
                crate::ElementKind::Way => way_schedule.push(entry),
                crate::ElementKind::Relation => relation_schedule.push(entry),
            }
        } else {
            // No indexdata - include in all schedules (conservative).
            node_blobs.push(NodeBlob {
                entry,
                passthrough: Passthrough::No,
            });
            way_schedule.push(entry);
            relation_schedule.push(entry);
        }
    }
    host.invocation.marker("MULTI_SCHEDULE_SCAN_END");
    // The classify pools take a bare schedule; derived, in order, so its
    // positions are `node_blobs`' positions by construction.
    let node_schedule: Vec<ScheduledBlob> = node_blobs.iter().map(|blob| blob.entry).collect();

    // DO NOT add per-region partial raw passthrough here (decode once,
    // write raw to contained regions, route elements to non-contained
    // regions). It was a "high impact at planet scale" item on the
    // multi-extract optimization plan for a while, resting on the
    // heuristic that region-contained blobs would cluster under
    // contiguous-bbox regions.
    //
    // Measured end-to-end on 2026-04-20 via a shadow counter that did
    // the full per-region bbox-containment classification per blob
    // without actually passing anything through raw. Shadow added in
    // the 2026-04-19 landing, reverted here. Planet 5-region
    // `--config --simple` at commit 57b01f9, UUID dad573cb:
    //
    //   0 / 32,835 node blobs would have qualified for partial
    //   passthrough. Zero. All 32,835 blobs (10.4 B elements) landed
    //   in the `none_contained` bucket - no region bbox contains any
    //   whole blob. The existing all-N-contained fast path also fired
    //   0 times.
    //
    // The math is hostile structurally, not a property of this region
    // set: PBF node blobs are ID-sorted, and OSM IDs are chronological
    // rather than geographic, so nodes in a single 8,000-element blob
    // scatter across the planet. Each blob's geographic bbox is
    // ~planet-wide, so it cannot be contained in a sub-planet region
    // under any bbox subdivision. Geography-sorted PBFs (Hilbert
    // curve over lat/lon) would flip this, but they break
    // Sort.Type_then_ID and are not what we ingest.
    //
    // The existing all-N-contained raw-passthrough path in
    // multi_extract_pread_write_nodes is kept for its real but narrow
    // niche: N=1 extracts and fully-overlapping regions. Don't remove
    // it on the "fires 0 times at planet 5-region" evidence - the
    // planet multi-region config is the hardest case, not the only
    // one.
    //
    // Sister pin: src/commands/tags_filter/mod.rs pass-2 worker (same
    // precedent: shadow measurement produced zero, reverted, leave the
    // pin as the block against re-attempting).

    let shared_file = walker.input();
    drop(walker);

    // Phase 1: Parallel node classification → N bbox_node_ids.
    // For all-bbox regions, use columnar decode (batch IDs/lats/lons into
    // contiguous arrays) with single-pass multi-region classification.
    // Polygon regions fall back to element-by-element iteration.
    let all_bbox = regions
        .iter()
        .all(|region| matches!(region, Region::Bbox(_)));
    use crate::tunables::GRID_REGION_THRESHOLD;
    let region_bboxes: Vec<(i32, i32, i32, i32)> = inside_bbox_ints
        .iter()
        .map(|bi| (bi.min_lat, bi.max_lat, bi.min_lon, bi.max_lon))
        .collect();
    let grid = if n >= GRID_REGION_THRESHOLD {
        // A test can decline the grid to drive the same config down the
        // linear scan and compare output byte for byte.
        #[cfg(feature = "test-hooks")]
        let grid = if host.invocation.faults().forces_linear_region_grid() {
            None
        } else {
            RegionGrid::build(&region_bboxes)
        };
        #[cfg(not(feature = "test-hooks"))]
        let grid = RegionGrid::build(&region_bboxes);
        if grid.is_none() {
            // Output is unchanged (the linear classifier tests every
            // region), but node classification cost grows with N.
            crate::host::diag!(
                host.diagnostics,
                Note,
                "extract.multi.grid_fallback",
                "Multi-extract region grid declined {n} regions (coverage over budget or an empty interior); classifying nodes against every region"
            );
            host.invocation.counter("multi_extract_grid_fallback", 1);
        }
        grid
    } else {
        None
    };
    host.invocation.marker("MULTI_NODE_CLASSIFY_START");
    // Observed node count per node-schedule position, for passthrough
    // candidates that verified. See `Passthrough`.
    let mut verified_nodes: Vec<Option<u64>> = vec![None; node_schedule.len()];
    if all_bbox {
        let bboxes = region_bboxes;
        // The one classifier for every node blob, candidate or not, so that
        // verification can never change which nodes are selected.
        // Fills `scratch` with each region's matching node ids, occurrences
        // kept (passthrough verification counts them).
        let classify_columns = |block: &PrimitiveBlock,
                                columns: &mut crate::read::columnar::DenseNodeColumns,
                                scratch: &mut Vec<Vec<i64>>| {
            block.decode_dense_columns(columns);
            for v in scratch.iter_mut() {
                v.clear();
            }
            match &grid {
                Some(grid) => {
                    columns.collect_matching_ids_multi_bbox_grid(&bboxes, grid, scratch);
                }
                None => columns.collect_matching_ids_multi_bbox(&bboxes, scratch),
            }
            // Dense columns deliberately exclude plain Node messages. Visit
            // only those messages here; keep dense classification batched.
            for group in block.groups() {
                for node in group.nodes() {
                    let lat = node.decimicro_lat();
                    let lon = node.decimicro_lon();
                    for (region, &(min_lat, max_lat, min_lon, max_lon)) in bboxes.iter().enumerate()
                    {
                        if lat >= min_lat && lat <= max_lat && lon >= min_lon && lon <= max_lon {
                            scratch[region].push(node.id());
                        }
                    }
                }
            }
        };
        let worker_init = || {
            (
                crate::read::columnar::DenseNodeColumns::new(),
                vec![Vec::<i64>::new(); n],
                BatchBuilder::new(n),
            )
        };

        // One pass over every node blob. Each is validated as block
        // construction validates it, refusing a contradicted kind claim
        // (a candidate is scheduled as nodes, and the classifier reads only
        // nodes), then built and classified. A passthrough candidate's
        // validated bytes are also read for the structural check in
        // `dense_only_payload` before the block is built: a block the decode
        // path rejects is never copied raw, and the check sees the payload,
        // not the offset tables a build appends. This one pass replaced a
        // block pool for the rest and a raw pool that copied each candidate
        // into a block; it is a simplification, measured neutral at planet
        // (reference/performance.md, "Payloads validated where they lie").
        let node_blobs_ref = &node_blobs;
        crate::scan::classify::parallel_project_phase(
            &shared_file,
            &node_schedule,
            None,
            worker_init,
            |position, buffer, (columns, scratch, builder)| {
                let (crate::read::block::NoProjection, validated) =
                    buffer.project(|_, _| crate::read::block::NoProjection)?;
                let eligible = if node_blobs_ref[position].passthrough == Passthrough::Candidate {
                    Some(dense_only_payload(validated.bytes())?)
                } else {
                    None
                };
                let block = validated.into_block();
                classify_columns(&block, columns, scratch);
                // Occurrence counts, not set sizes: a duplicate id counts
                // every time it is copied.
                let observed = columns.len();
                let verified = (eligible == Some(true)
                    && scratch.iter().all(|ids| ids.len() == observed))
                .then_some(observed as u64);
                for (region, ids) in scratch.iter().enumerate() {
                    for &id in ids {
                        builder.push(id, region);
                    }
                }
                Ok((builder.finish(), verified))
            },
            |pos, (batch, verified): (PageBatch, Option<u64>)| {
                node_members.install(batch);
                verified_nodes[pos] = verified;
            },
            host,
        )?;
    } else {
        crate::scan::classify::parallel_classify_phase(
            &shared_file,
            &node_schedule,
            None,
            || BatchBuilder::new(n),
            |block, builder: &mut BatchBuilder| {
                for element in block.elements_skip_metadata() {
                    let (id, lat, lon) = match &element {
                        Element::DenseNode(node) => {
                            (node.id(), node.decimicro_lat(), node.decimicro_lon())
                        }
                        Element::Node(node) => {
                            (node.id(), node.decimicro_lat(), node.decimicro_lon())
                        }
                        _ => continue,
                    };
                    match &grid {
                        Some(grid) => {
                            for &region in grid.candidates(lat, lon) {
                                let region = region as usize;
                                if regions[region].contains_decimicro(
                                    &inside_bbox_ints[region],
                                    lat,
                                    lon,
                                ) {
                                    builder.push(id, region);
                                }
                            }
                        }
                        None => {
                            for region in 0..n {
                                if regions[region].contains_decimicro(
                                    &inside_bbox_ints[region],
                                    lat,
                                    lon,
                                ) {
                                    builder.push(id, region);
                                }
                            }
                        }
                    }
                }
                Ok(builder.finish())
            },
            |_seq, batch: PageBatch| node_members.install(batch),
            host,
        )?;
    }
    node_members.freeze();
    host.invocation.marker("MULTI_NODE_CLASSIFY_END");

    // Settle every candidate: verified ones pass through counted by what
    // classify observed, the rest are decoded like any other blob.
    let mut verified_blobs: u64 = 0;
    let mut demoted_blobs: u64 = 0;
    for (info, verified) in node_blobs.iter_mut().zip(&verified_nodes) {
        if info.passthrough == Passthrough::Candidate {
            info.passthrough = match *verified {
                Some(nodes) => {
                    verified_blobs += 1;
                    Passthrough::Verified { nodes }
                }
                None => {
                    demoted_blobs += 1;
                    Passthrough::No
                }
            };
        }
    }
    warn_passthrough_demotions(verified_blobs, demoted_blobs, host);

    // Phase 1 write: parallel decode with raw passthrough for fully-contained node blobs.
    host.invocation.marker("MULTI_NODE_WRITE_START");
    multi_extract_pread_write_nodes(
        &shared_file,
        &node_blobs,
        n,
        |block, bbs, output, _scratch| {
            let mut counts = vec![0u64; n];
            let mut mask = vec![0u64; mw];
            for element in block.elements() {
                match &element {
                    // A non-member skips metadata decode: the overwhelmingly
                    // common no-region-matches node.
                    Element::DenseNode(dn) if node_members.mask_into(dn.id(), &mut mask) => {
                        let id = dn.id();
                        let meta = dense_node_metadata(dn)?;
                        for i in regions_of(&mask) {
                            ensure_node_capacity_local(&mut bbs[i], &mut output[i])?;
                            bbs[i].add_node(
                                id,
                                dn.decimicro_lat(),
                                dn.decimicro_lon(),
                                dn.tags(),
                                meta.as_ref(),
                            );
                            counts[i] += 1;
                        }
                    }
                    Element::Node(nd) if node_members.mask_into(nd.id(), &mut mask) => {
                        let id = nd.id();
                        let meta = element_metadata(ElementKind::Node, id, &nd.info())?;
                        for i in regions_of(&mask) {
                            ensure_node_capacity_local(&mut bbs[i], &mut output[i])?;
                            bbs[i].add_node(
                                id,
                                nd.decimicro_lat(),
                                nd.decimicro_lon(),
                                nd.tags(),
                                meta.as_ref(),
                            );
                            counts[i] += 1;
                        }
                    }
                    _ => {}
                }
            }
            Ok(counts)
        },
        &framing,
        &mut writers,
        &mut stats,
        host,
    )?;
    host.invocation.marker("MULTI_NODE_WRITE_END");

    // Phase 2: Parallel way classification → way membership.
    //
    // Per-worker scratch (the batch builder and the way's mask) persists
    // across blocks so its capacity amortizes across the blobs each worker
    // processes. Pre-instrumentation Japan 5-region bench showed this phase
    // at 892 ms with per-block scratch allocation.
    //
    // A way's mask is the OR of its refs' node masks; the walk stops once
    // every region has matched.
    //
    // Deliberately a block walk, not a projection. Not measured here: the
    // estimate rests on extract smart's way classify, the same shape (every
    // way's refs tested against a node set), which stayed at the same
    // length on `read::project_ids::WaySpans` at planet. This loop adds the
    // per-region membership tests on top, so the message re-parse a
    // projection would remove is a smaller share still
    // (reference/performance.md, "inspect --nodes reads through a
    // projection").
    host.invocation.marker("MULTI_WAY_CLASSIFY_START");
    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &way_schedule,
        None,
        || (BatchBuilder::new(n), vec![0u64; mw]),
        |block, (builder, acc): &mut (BatchBuilder, Vec<u64>)| {
            for element in block.elements_skip_metadata() {
                if let Element::Way(w) = &element {
                    acc.fill(0);
                    for node_id in w.refs() {
                        if node_members.or_into(node_id, acc) && *acc == all_regions {
                            break;
                        }
                    }
                    builder.push_mask(w.id(), acc);
                }
            }
            Ok(builder.finish())
        },
        |_seq, batch: PageBatch| way_members.install(batch),
        host,
    )?;
    way_members.freeze();
    host.invocation.marker("MULTI_WAY_CLASSIFY_END");

    // Phase 2 write: parallel decode, write matching ways to N writers.
    host.invocation.marker("MULTI_WAY_WRITE_START");
    multi_extract_pread_write(
        &shared_file,
        &way_schedule,
        n,
        |block, bbs, output, scratch| {
            let mut counts = vec![0u64; n];
            let mut mask = vec![0u64; mw];
            for element in block.elements() {
                if let Element::Way(w) = &element {
                    let wid = w.id();
                    if !way_members.mask_into(wid, &mut mask) {
                        continue;
                    }
                    scratch.clear();
                    scratch.extend(w.refs());
                    let meta = element_metadata(ElementKind::Way, wid, &w.info())?;
                    for i in regions_of(&mask) {
                        ensure_way_capacity_local(&mut bbs[i], &mut output[i])?;
                        bbs[i].add_way(wid, w.tags(), scratch, meta.as_ref());
                        counts[i] += 1;
                    }
                }
            }
            Ok(counts)
        },
        &framing,
        &mut writers,
        &mut stats,
        |s| &mut s.ways_written,
        host,
    )?;
    host.invocation.marker("MULTI_WAY_WRITE_END");

    // Phase 3: Parallel relation classification → relation membership.
    //
    // A relation's mask is the OR of its node members' and way members'
    // masks (the member rule `relation_has_matched_member` applies per
    // region). Streamed per blob like the other kinds: classification reads
    // only the finished node and way memberships.
    //
    // A block walk on purpose: the phase is under 1 s at planet, so the
    // re-parse a projection would remove sits under the command's noise
    // floor.
    host.invocation.marker("MULTI_REL_CLASSIFY_START");
    crate::scan::classify::parallel_classify_phase(
        &shared_file,
        &relation_schedule,
        None,
        || (BatchBuilder::new(n), vec![0u64; mw]),
        |block, (builder, acc): &mut (BatchBuilder, Vec<u64>)| {
            for element in block.elements_skip_metadata() {
                if let Element::Relation(r) = &element {
                    acc.fill(0);
                    for member in matchable_members(r) {
                        let hit = match member {
                            MatchableMember::Node(id) => node_members.or_into(id, acc),
                            MatchableMember::Way(id) => way_members.or_into(id, acc),
                        };
                        if hit && *acc == all_regions {
                            break;
                        }
                    }
                    builder.push_mask(r.id(), acc);
                }
            }
            Ok(builder.finish())
        },
        |_seq, batch: PageBatch| relation_members.install(batch),
        host,
    )?;
    relation_members.freeze();
    host.invocation.marker("MULTI_REL_CLASSIFY_END");

    // Phase 3 write: parallel decode, write matching relations to N writers.
    host.invocation.marker("MULTI_REL_WRITE_START");
    multi_extract_pread_write(
        &shared_file,
        &relation_schedule,
        n,
        |block, bbs, output, _scratch| {
            let mut counts = vec![0u64; n];
            let mut members_buf: Vec<MemberData<'_>> = Vec::new();
            let mut mask = vec![0u64; mw];
            for element in block.elements() {
                if let Element::Relation(r) = &element {
                    let rid = r.id();
                    if !relation_members.mask_into(rid, &mut mask) {
                        continue;
                    }
                    relation_members_into(r, &mut members_buf)?;
                    let meta = element_metadata(ElementKind::Relation, rid, &r.info())?;
                    for i in regions_of(&mask) {
                        ensure_relation_capacity_local(&mut bbs[i], &mut output[i])?;
                        bbs[i].add_relation(rid, r.tags(), &members_buf, meta.as_ref());
                        counts[i] += 1;
                    }
                }
            }
            Ok(counts)
        },
        &framing,
        &mut writers,
        &mut stats,
        |s| &mut s.relations_written,
        host,
    )?;
    host.invocation.marker("MULTI_REL_WRITE_END");

    // Flush all writers (workers already flushed their BlockBuilders per blob).
    for (writer, output) in writers.iter_mut().zip(outputs.iter()) {
        let display_path = output.destination();
        writer
            .flush()
            .map_err(|e| e.context(format!("failed to flush {}", display_path.display())))?;
    }

    // Counters: schedule sizes + cross-region totals. Parallels the single-region
    // `extract()` wrapper at the bottom of `pub fn extract` - the single-pass
    // path bypasses that wrapper, so without this block `brokkr sidecar
    // --counters` is empty for multi-extract runs.
    host.invocation.counter("multi_extract_region_count", n);
    host.invocation
        .counter("multi_extract_node_blobs", node_schedule.len());
    host.invocation
        .counter("multi_extract_way_blobs", way_schedule.len());
    host.invocation
        .counter("multi_extract_relation_blobs", relation_schedule.len());
    let total_nodes: u64 = stats.iter().map(|s| s.nodes_in_bbox).sum();
    let total_ways: u64 = stats.iter().map(|s| s.ways_written).sum();
    let total_relations: u64 = stats.iter().map(|s| s.relations_written).sum();
    host.invocation
        .counter("multi_extract_nodes_written", total_nodes);
    host.invocation
        .counter("multi_extract_ways_written", total_ways);
    host.invocation
        .counter("multi_extract_relations_written", total_relations);
    host.invocation.marker("MULTI_EXTRACT_END");

    Ok(Some(stats))
}

// ---------------------------------------------------------------------------
// Parallel batch infrastructure
// ---------------------------------------------------------------------------

/// One node blob of the node pass: its schedule entry (which also locates
/// its raw frame for passthrough) and its passthrough state.
struct NodeBlob {
    entry: ScheduledBlob,
    /// `Candidate` when the indexdata bbox claims containment in every
    /// region; settled to `Verified` or `No` after node classify.
    passthrough: Passthrough,
}

/// Node write phase with raw passthrough for verified blobs.
///
/// Blobs verified as fully contained in ALL N regions are written as raw
/// frames to all N writers without decode, counted by the node count
/// classify observed. Other blobs go through parallel decode workers.
/// Workers compress and frame the decoded blobs' per-region blocks; both
/// streams are interleaved in sequence order by the ordered pool.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn multi_extract_pread_write_nodes<F>(
    input: &InputFile,
    blobs: &[NodeBlob],
    n: usize,
    block_fn: F,
    framing: &RegionFraming<'_>,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
    host: &crate::host::Entered<'_>,
) -> Result<()>
where
    F: Fn(
            &PrimitiveBlock,
            &mut Vec<BlockBuilder>,
            &mut Vec<Vec<OwnedBlock>>,
            &mut Vec<i64>,
        ) -> crate::Result<Vec<u64>>
        + Send
        + Sync,
{
    // Blobs verified as fully contained in ALL N regions skip decode
    // entirely - the consumer writes their raw frame to all N writers
    // between the decoded ones. Other blobs, including any unsettled
    // candidate, go through the parallel decode workers.
    let decode_at = crate::ordered_pool::DecodeSchedule::from_items(blobs, |blob| {
        !matches!(blob.passthrough, Passthrough::Verified { .. })
    });

    let pt = decode_at.passthrough_count();
    if pt > 0 {
        let dc = decode_at.decode_count();
        crate::host::diag!(
            host.diagnostics,
            Progress,
            "extract.multi.node_blobs",
            "  node blobs: {pt} passthrough, {dc} decoded"
        );
    }

    let decode_threads = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    );
    let decode_at = &decode_at;
    let mut consumer = MultiNodeConsumer {
        interleaved: crate::ordered_pool::Interleaved::new(decode_at),
        blobs,
        input,
        frame_buf: crate::read::read_buf::ReadBuf::default(),
        writers,
        stats,
        cancel: &host.invocation,
    };
    let mut workers = vec![(); decode_threads];
    crate::ordered_pool::run_ordered(
        consumer.interleaved.decode_count(),
        &mut workers,
        crate::ordered_pool::PoolConfig {
            window: crate::ordered_pool::default_window(decode_threads),
            cancel: &host.invocation,
            waits: crate::ordered_pool::Waits::default(),
        },
        |_| MultiLocal::new(n, &host.invocation),
        |(), local: &mut MultiLocal, ordinal| {
            let entry = &blobs[decode_at.position(ordinal)].entry;
            let block = local.decode(input, entry, &host.invocation)?;
            local
                .classify(&block, n, &block_fn, framing)
                .map_err(|e| input.locate_blob(entry, e))
        },
        &mut consumer,
    )?;
    for position in consumer.interleaved.rest() {
        consumer.passthrough(position)?;
    }
    Ok(())
}

/// One multi-extract worker's thread-local state: per-region builders and
/// output, plus the decode scratch.
struct MultiLocal {
    read_buf: crate::read::read_buf::ReadBuf,
    bbs: Vec<BlockBuilder>,
    output: Vec<Vec<OwnedBlock>>,
    pool: std::sync::Arc<crate::blob::DecompressPool>,
    st_scratch: Vec<(u32, u32)>,
    gr_scratch: Vec<(u32, u32)>,
    i64_scratch: Vec<i64>,
}

impl MultiLocal {
    fn new(n: usize, invocation: &crate::host::Recorder) -> Self {
        Self {
            read_buf: crate::read::read_buf::ReadBuf::default(),
            bbs: (0..n).map(|_| BlockBuilder::new()).collect(),
            output: (0..n).map(|_| Vec::new()).collect(),
            pool: crate::blob::DecompressPool::new(invocation),
            st_scratch: Vec::new(),
            gr_scratch: Vec::new(),
            i64_scratch: Vec::new(),
        }
    }

    /// pread, decompress and parse `entry`, located with the input on failure.
    /// The read buffer follows its retention limit once the blob is
    /// decompressed (read/read_buf.rs).
    fn decode(
        &mut self,
        input: &InputFile,
        entry: &ScheduledBlob,
        invocation: &crate::host::Recorder,
    ) -> Result<PrimitiveBlock> {
        let block = (|| {
            let raw = self.read_buf.read_exact_at(
                &**input.file(),
                entry.data_offset(),
                entry.data_size(),
            )?;
            let mut buf = crate::blob::pool_get(Some(&self.pool), 0);
            crate::blob::decompress_blob_raw(raw, &mut buf)?;
            PrimitiveBlock::from_vec_pooled_with_scratch(
                buf,
                &self.pool,
                &mut self.st_scratch,
                &mut self.gr_scratch,
            )
        })()
        .map_err(|e| input.locate_blob(entry, e));
        self.read_buf.release_outlier(invocation);
        block
    }

    /// Classify `block` against the N regions and take each region's blocks.
    fn classify<F>(
        &mut self,
        block: &PrimitiveBlock,
        n: usize,
        block_fn: &F,
        framing: &RegionFraming<'_>,
    ) -> crate::Result<RegionFrames>
    where
        F: Fn(
            &PrimitiveBlock,
            &mut Vec<BlockBuilder>,
            &mut Vec<Vec<OwnedBlock>>,
            &mut Vec<i64>,
        ) -> crate::Result<Vec<u64>>,
    {
        for v in &mut self.output {
            v.clear();
        }
        let counts = block_fn(
            block,
            &mut self.bbs,
            &mut self.output,
            &mut self.i64_scratch,
        )?;
        for i in 0..n {
            flush_local(&mut self.bbs[i], &mut self.output[i])?;
        }
        let mut frames = Vec::with_capacity(n);
        for (i, blocks) in self.output.iter_mut().enumerate() {
            frames.push(framing.frame_region(i, blocks.drain(..))?);
        }
        Ok((frames, counts))
    }
}

/// One task's output: each region's framed blobs, in region order and
/// block order within a region, and each region's element count.
type RegionFrames = (Vec<Vec<Vec<u8>>>, Vec<u64>);

/// Compression and framing of the regions' blocks inside the pool's tasks.
///
/// The bytes are the sync writer's own: both go through the one framing
/// core with the block's given index, tagdata and way members. A frame
/// travels as one whole owned buffer, the pipelined framing's shape.
///
/// The pool's window is the regions' one shared bound: a task carries every
/// region's frames for its input blob and returns its credit only once the
/// consumer has handed all of them to the writers. It counts tasks, not
/// bytes; a slot holds that blob's whole framed fanout.
struct RegionFraming<'a> {
    compression: Compression,
    destinations: Vec<&'a std::path::Path>,
    invocation: &'a crate::host::Recorder,
}

impl RegionFraming<'_> {
    /// Frame region `region`'s blocks, consuming each as it is framed so a
    /// task never holds the uncompressed and framed copies of all its
    /// blocks at once. Checks cancellation before each frame; a framing
    /// error names the region's output.
    fn frame_region(
        &self,
        region: usize,
        blocks: impl Iterator<Item = OwnedBlock>,
    ) -> Result<Vec<Vec<u8>>> {
        let metrics = &self.invocation.metrics().writer;
        let mut frames = Vec::new();
        for block in blocks {
            self.invocation.check()?;
            let OwnedBlock {
                bytes,
                index,
                tagdata,
                way_members,
            } = block;
            let indexdata = index.serialize();
            let frame = crate::writer::frame_blob_pipelined(
                &bytes,
                &self.compression,
                Some(indexdata.as_slice()),
                tagdata.as_deref(),
                way_members.as_deref(),
                metrics,
            )
            .map_err(|e| e.writing(self.destinations[region]))?;
            drop(bytes);
            frames.push(frame);
        }
        Ok(frames)
    }
}

/// Write one decoded blob's per-region frames to the N writers, adding each
/// region's count through `stat_field`.
fn write_regions(
    (region_frames, counts): RegionFrames,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
    stat_field: fn(&mut ExtractStats) -> &mut u64,
) -> Result<()> {
    for (i, frames) in region_frames.into_iter().enumerate() {
        *stat_field(&mut stats[i]) += counts[i];
        for frame in frames {
            writers[i].write_raw_owned(frame)?;
        }
    }
    Ok(())
}

/// The node pass's consumer: decoded per-region blocks and verified
/// passthrough frames, merged in schedule order.
///
/// A raw frame keeps the input's BlobHeader, sidecars included: verified
/// passthrough proved region membership, not that the header is true, so a
/// false `count` or ID range carries into every region's output (see the
/// matching note in `common::pread_execute`).
struct MultiNodeConsumer<'a> {
    interleaved: crate::ordered_pool::Interleaved<'a>,
    blobs: &'a [NodeBlob],
    input: &'a InputFile,
    frame_buf: crate::read::read_buf::ReadBuf,
    writers: &'a mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &'a mut [ExtractStats],
    cancel: &'a crate::host::Recorder,
}

impl MultiNodeConsumer<'_> {
    /// Copy the verified raw frame at schedule position `position` to all N
    /// writers.
    fn passthrough(&mut self, position: usize) -> Result<()> {
        self.cancel.check()?;
        let blob = &self.blobs[position];
        let Passthrough::Verified { nodes } = blob.passthrough else {
            return Err(crate::error::Error::internal(
                "multi-extract: a decode blob was scheduled as passthrough",
            ));
        };
        let frame_start = blob.entry.frame_start();
        let frame = self
            .frame_buf
            .read_exact_at(&**self.input.file(), frame_start, blob.entry.frame_size())
            .map_err(|e| self.input.locate_at(frame_start, e.into()))?;
        for (writer, stats) in self.writers.iter_mut().zip(self.stats.iter_mut()) {
            writer.write_raw(frame)?;
            stats.nodes_in_bbox += nodes;
        }
        // Every region has its copy; the frame buffer follows its retention
        // limit (read/read_buf.rs).
        self.frame_buf.release_outlier(self.cancel);
        Ok(())
    }
}

impl crate::ordered_pool::OrderedConsumer<RegionFrames> for MultiNodeConsumer<'_> {
    fn before_item(&mut self, ordinal: usize) -> crate::Result<()> {
        for position in self.interleaved.before(ordinal) {
            self.passthrough(position)?;
        }
        Ok(())
    }

    fn consume(&mut self, _ordinal: usize, item: RegionFrames) -> crate::Result<()> {
        write_regions(item, self.writers, self.stats, |s| &mut s.nodes_in_bbox)
    }
}

/// Multi-region pread-from-workers write pass.
///
/// Workers pread blob data, decompress, parse into PrimitiveBlock, then call
/// the provided closure to classify elements against N regions and produce
/// N × Vec<OwnedBlock>, then compress and frame those blocks
/// (`RegionFraming`). The consumer copies each region's frames to its
/// writer in sequence order.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
fn multi_extract_pread_write<F>(
    input: &InputFile,
    schedule: &[ScheduledBlob],
    n: usize,
    block_fn: F,
    framing: &RegionFraming<'_>,
    writers: &mut [PbfWriter<std::io::BufWriter<std::fs::File>>],
    stats: &mut [ExtractStats],
    stat_field: fn(&mut ExtractStats) -> &mut u64,
    host: &crate::host::Entered<'_>,
) -> Result<()>
where
    F: Fn(
            &PrimitiveBlock,
            &mut Vec<BlockBuilder>,
            &mut Vec<Vec<OwnedBlock>>,
            &mut Vec<i64>,
        ) -> crate::Result<Vec<u64>>
        + Send
        + Sync,
{
    // The ordered classify pool: per-region results in schedule order,
    // bounded by its window. Each worker keeps its per-region builders,
    // output vectors and ID scratch across blobs, built on its own thread.
    crate::scan::classify::parallel_classify_phase_ordered(
        input,
        schedule,
        None,
        None,
        || MultiLocal::new(n, &host.invocation),
        |block, local: &mut MultiLocal| local.classify(block, n, &block_fn, framing),
        |_position, item| write_regions(item, writers, stats, stat_field),
        host,
    )
}
