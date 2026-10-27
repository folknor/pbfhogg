//! Reverse lookup: find ways/relations referencing given IDs. Equivalent to `osmium getparents`.
//!
//! HeaderWalker-driven schedule: pread only the blob kinds whose bodies can
//! contribute matches. Workers decode and scan; a reorder buffer delivers
//! owned blocks to the writer in file order.
//!
//! Node blobs are skipped unless `--add-self` matches a node ID, saving the
//! ~75 % of planet bytes that nodes occupy. Way blobs are skipped when no
//! node IDs are in the query (nothing to ref), and relation blobs when the
//! query is empty of node/way/relation IDs.

use super::getid::ElementIds;
use super::{
    HeaderOverrides, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local, writer_from_header_bytes,
};
use crate::ElementKind;
use crate::blob::{BlobKind, decode_blob_to_headerblock};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::owned::{dense_node_metadata, element_metadata, relation_members_into};
use crate::read::header_walker::{HeaderWalker, ScanArm};
use crate::read::input::{InputSource, PinnedInput};
use crate::tunables::FULL_SCAN_ARM_MIN_BLOBS;
use crate::write::WriteOptions;
use crate::{BlobFilter, Element, ElementReader, MemberId, PrimitiveBlock};

use super::Result;

/// Options for the getparents command.
pub struct GetparentsOptions {
    /// Also include the queried objects themselves in the output.
    pub add_self: bool,
}

/// Statistics from a getparents operation.
pub struct GetparentsStats {
    pub nodes_written: u64,
    pub ways_written: u64,
    pub relations_written: u64,
}

impl GetparentsStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let total = self.nodes_written + self.ways_written + self.relations_written;
        writeln!(
            w,
            "Wrote {total} elements: {} nodes, {} ways, {} relations",
            self.nodes_written, self.ways_written, self.relations_written,
        )?;

        Ok(())
    }
}

/// Find parent ways/relations referencing the given IDs.
///
/// Every route opens the input more than once (the indexdata probe, the
/// arm estimate on indexed input, then the header walker or the streaming
/// reader), so the input must be a regular file
/// (`InvalidArgument` otherwise) and is pinned for the run before any of
/// them (see [`PinnedInput`] and [`InputSource::pin`]); a caller may pin it
/// first and pass the pin. Where a later open cannot reach the pinned file,
/// an input replaced mid-run fails with `InputReplaced`.
pub fn getparents<'a>(
    input: impl Into<InputSource<'a>>,
    output: crate::output::Output,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<GetparentsStats> {
    host.invocation.check()?;
    let result = getparents_to(input, &output, ids, opts, write, overrides, host)?;
    output.publish(&host.invocation)?;
    Ok(result)
}

/// [`getparents`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn getparents_to<'a>(
    input: impl Into<InputSource<'a>>,
    output: &crate::output::Output,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<GetparentsStats> {
    let (_, stats) = getparents_dispatched(
        input,
        output,
        ids,
        opts,
        write,
        overrides,
        FULL_SCAN_ARM_MIN_BLOBS,
        host,
    )?;
    Ok(stats)
}

/// Test instrument for exercising the FullScan arm on small fixtures: runs
/// [`getparents`] with the blob-count dispatch threshold replaced by
/// `min_blobs`. The input is pinned as in [`getparents`].
///
/// Not part of the supported library surface: it exists for this crate's
/// own tests and for the CLI's hidden `--full-scan-min-blobs` flag, and may
/// change or disappear without a deprecation cycle. It is public only
/// because those callers live in other crates. The threshold is otherwise a
/// constant, not a tuning knob (see `src/tunables.rs`).
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn getparents_with_min_blobs<'a>(
    input: impl Into<InputSource<'a>>,
    output: crate::output::Output,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    min_blobs: u64,
    host: &crate::host::Host,
) -> Result<GetparentsStats> {
    host.invocation.check()?;
    let result =
        getparents_with_min_blobs_to(input, &output, ids, opts, write, overrides, min_blobs, host)?;
    output.publish(&host.invocation)?;
    Ok(result)
}

/// [`getparents_with_min_blobs`] writing to a borrowed output it does not publish.
#[allow(clippy::too_many_arguments)]
pub(crate) fn getparents_with_min_blobs_to<'a>(
    input: impl Into<InputSource<'a>>,
    output: &crate::output::Output,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    min_blobs: u64,
    host: &crate::host::Host,
) -> Result<GetparentsStats> {
    getparents_dispatched(input, output, ids, opts, write, overrides, min_blobs, host)
        .map(|(_, stats)| stats)
}

/// Pin the input, dispatch on the blob-count estimate, then run the
/// selected arm. Returns the arm alongside the stats so tests can inject
/// `min_blobs` and assert which arm auto-dispatch actually executed.
#[allow(clippy::too_many_arguments)]
fn getparents_dispatched<'a>(
    input: impl Into<InputSource<'a>>,
    output: &crate::output::Output,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    min_blobs: u64,
    host: &crate::host::Host,
) -> Result<(ScanArm, GetparentsStats)> {
    // Every route opens the input more than once, each open its own open
    // file description: the indexdata probe, the arm estimate (indexed
    // input only), then the header walker (whose descriptor serves the
    // classify pool) or the streaming reader. Each is derived from this one
    // pin, so a file renamed over the input mid-run cannot be read partly
    // from each: where `/proc/self/fd` reopens the pin every open reaches
    // the pinned file, otherwise on Unix a replaced input fails the run
    // (`PinnedInput` says what it does off Unix). Taken before any other
    // open of the input; a caller's pin is used as it is.
    let pin = input.into().pin(host)?;
    let input = &pin;
    let arm = super::dispatch_scan_arm(
        input,
        super::has_indexdata_in(input, write.direct_reads, host)?,
        min_blobs,
        host,
    )?;
    let stats = getparents_with_arm(input, output, ids, opts, write, overrides, arm, host)?;
    Ok((arm, stats))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn getparents_with_arm(
    input: &PinnedInput,
    output: &crate::output::Output,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    arm: ScanArm,
    host: &crate::host::Host,
) -> Result<GetparentsStats> {
    match arm {
        ScanArm::Walker => getparents_walker(input, output, ids, opts, write, overrides, host),
        ScanArm::FullScan => getparents_pipelined(input, output, ids, opts, write, overrides, host),
    }
}

/// Which blob kinds can contribute matches?
/// - node blobs: only for --add-self on node IDs
/// - way blobs: any way may reference a query node ID; --add-self on way IDs
/// - relation blobs: any relation may reference a query node/way/relation ID;
///   --add-self on relation IDs
fn needed_blob_kinds(ids: &ElementIds, opts: &GetparentsOptions) -> (bool, bool, bool) {
    (
        opts.add_self && ids.node_ids.has_any(),
        ids.node_ids.has_any() || (opts.add_self && ids.way_ids.has_any()),
        ids.node_ids.has_any() || ids.way_ids.has_any() || ids.relation_ids.has_any(),
    )
}

#[allow(clippy::too_many_lines)]
fn getparents_walker(
    input: &PinnedInput,
    output: &crate::output::Output,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<GetparentsStats> {
    let (need_node_blobs, need_way_blobs, need_relation_blobs) = needed_blob_kinds(ids, opts);

    host.invocation.marker("GETPARENTS_SCHEDULE_START");
    let mut walker = HeaderWalker::open(input, host)?;
    let mut header_buf: Vec<u8> = Vec::new();
    let mut header_block: Option<crate::HeaderBlock> = None;
    let mut schedule: Vec<crate::scan::classify::ScheduledBlob> = Vec::new();
    let mut blobs_skipped: u64 = 0;

    while let Some(meta) = walker.next_header()? {
        match meta.blob_type {
            BlobKind::OsmHeader if header_block.is_none() => {
                walker
                    .pread_data(meta.data_offset, meta.data_size, &mut header_buf)
                    .map_err(|e| walker.locate_meta(&meta, e))?;
                header_block = Some(
                    decode_blob_to_headerblock(&header_buf)
                        .map_err(|e| walker.locate_meta(&meta, e))?,
                );
            }
            BlobKind::OsmData => {
                // A kind filter only prunes when some kind is unwanted.
                if !(need_node_blobs && need_way_blobs && need_relation_blobs) {
                    walker.observe_filter_index(meta.index.as_ref());
                }
                let keep = match meta.index.as_ref().map(|i| i.kind) {
                    Some(ElementKind::Node) => need_node_blobs,
                    Some(ElementKind::Way) => need_way_blobs,
                    Some(ElementKind::Relation) => need_relation_blobs,
                    // Unindexed blob: include conservatively since we don't
                    // know its kind without decoding.
                    None => true,
                };
                if keep {
                    // The walk has already refused a payload running past
                    // EOF, so the schedule's preads stay in the file.
                    schedule.push(crate::scan::classify::ScheduledBlob::from_meta(
                        &meta,
                        crate::scan::classify::KindClaims::Refuse,
                    )?);
                } else {
                    blobs_skipped += 1;
                }
            }
            _ => {}
        }
    }

    host.invocation.counter(
        "walk_actual_osmdata_blobs",
        u64::try_from(schedule.len())
            .unwrap_or(u64::MAX)
            .saturating_add(blobs_skipped),
    );

    let shared_file = walker.input();
    drop(walker);

    let header = header_block.ok_or_else(|| crate::error::new_missing_header(input.path()))?;

    host.invocation
        .counter("getparents_schedule_blobs", schedule.len());
    host.invocation
        .counter("getparents_blobs_skipped", blobs_skipped);
    host.invocation.marker("GETPARENTS_SCHEDULE_END");

    let header_bytes = super::output_header::build_one(
        &header,
        &super::output_header::OutputFacts::carry(super::output_header::Ways::Reencoded),
        overrides,
        host,
    )?
    .bytes;
    let mut writer = writer_from_header_bytes(
        output,
        write.compression,
        &header_bytes,
        write.backend,
        host,
    )?;

    let mut stats = GetparentsStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    host.invocation.marker("GETPARENTS_DECODE_START");
    type ClassifyResult = crate::Result<(Vec<OwnedBlock>, (u64, u64, u64))>;

    // Results arrive in schedule order through the ordered pool, whose window
    // bounds the decoded output in flight. A write error ends the scan at
    // once (at planet scale, finishing the decode first would cost the whole
    // run before the error surfaced).
    crate::scan::classify::parallel_classify_phase_ordered(
        &shared_file,
        &schedule,
        None,
        None,
        // `BlockBuilder` contains `Rc<str>` (string table) and is not
        // Send, so it can't live as worker state across
        // `parallel_classify_phase`'s `S: Send` bound. Instead, each
        // classify call materialises its own builder, flushing into a
        // per-blob `Vec<OwnedBlock>` before returning.
        || (),
        |block, _state| -> ClassifyResult {
            let mut bb = BlockBuilder::new();
            let mut output: Vec<OwnedBlock> = Vec::new();
            let counts = process_block(block, &mut bb, &mut output, ids, opts.add_self)?;
            flush_local(&mut bb, &mut output)?;
            Ok((output, counts))
        },
        |_position, (blocks, (n, w, r))| {
            for block in blocks {
                writer.write_primitive_block_owned(block)?;
            }
            stats.nodes_written += n;
            stats.ways_written += w;
            stats.relations_written += r;
            Ok(())
        },
        host,
    )?;

    writer.flush()?;
    host.invocation.marker("GETPARENTS_DECODE_END");
    Ok(stats)
}

fn getparents_pipelined(
    input: &PinnedInput,
    output: &crate::output::Output,
    ids: &ElementIds,
    opts: &GetparentsOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<GetparentsStats> {
    let (need_node_blobs, need_way_blobs, need_relation_blobs) = needed_blob_kinds(ids, opts);
    let reader = ElementReader::open_in(input, write.direct_reads, host)?
        .refusing_kind_claims()
        .with_blob_filter(BlobFilter::new(
            need_node_blobs,
            need_way_blobs,
            need_relation_blobs,
        ));
    let header_bytes = super::output_header::build_one(
        reader.header(),
        &super::output_header::OutputFacts::carry(super::output_header::Ways::Reencoded),
        overrides,
        host,
    )?
    .bytes;
    let mut writer = writer_from_header_bytes(
        output,
        write.compression,
        &header_bytes,
        write.backend,
        host,
    )?;
    let mut stats = GetparentsStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
    };

    host.invocation.marker("GETPARENTS_DECODE_START");
    reader.for_each_fused_block(
        |block| {
            let mut bb = BlockBuilder::new();
            let mut output = Vec::new();
            let counts = process_block(&block, &mut bb, &mut output, ids, opts.add_self)?;
            flush_local(&mut bb, &mut output)?;
            Ok((output, counts))
        },
        |(blocks, (nodes, ways, relations))| {
            for block in blocks {
                writer.write_primitive_block_owned(block)?;
            }
            stats.nodes_written += nodes;
            stats.ways_written += ways;
            stats.relations_written += relations;
            Ok(())
        },
    )?;
    writer.flush()?;
    host.invocation.marker("GETPARENTS_DECODE_END");
    Ok(stats)
}

fn process_block(
    block: &PrimitiveBlock,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
    ids: &ElementIds,
    add_self: bool,
) -> crate::Result<(u64, u64, u64)> {
    let mut nodes: u64 = 0;
    let mut ways: u64 = 0;
    let mut relations: u64 = 0;

    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        match &element {
            Element::DenseNode(dn) => {
                // Nodes are never parents. Include only if --add-self and ID matches.
                if add_self && ids.node_ids.get(dn.id()) {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = dense_node_metadata(dn)?;
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        dn.tags(),
                        meta.as_ref(),
                    );
                    nodes += 1;
                }
            }
            Element::Node(n) => {
                if add_self && ids.node_ids.get(n.id()) {
                    ensure_node_capacity_local(bb, output)?;
                    let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        n.tags(),
                        meta.as_ref(),
                    );
                    nodes += 1;
                }
            }
            Element::Way(w) => {
                // A way is a parent if it references any requested node ID.
                let is_parent = w.refs().any(|r| ids.node_ids.get(r));
                let is_self = add_self && ids.way_ids.get(w.id());
                if is_parent || is_self {
                    ensure_way_capacity_local(bb, output)?;
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(ElementKind::Way, w.id(), &w.info())?;
                    bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                    ways += 1;
                }
            }
            Element::Relation(r) => {
                // A relation is a parent if any member matches a requested ID.
                let is_parent = r.members().any(|m| match m.id {
                    MemberId::Node(id) => ids.node_ids.get(id),
                    MemberId::Way(id) => ids.way_ids.get(id),
                    MemberId::Relation(id) => ids.relation_ids.get(id),
                    MemberId::Unknown(..) => false,
                });
                let is_self = add_self && ids.relation_ids.get(r.id());
                if is_parent || is_self {
                    ensure_relation_capacity_local(bb, output)?;
                    relation_members_into(r, &mut members_buf)?;
                    let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                    bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                    relations += 1;
                }
            }
        }
    }

    Ok((nodes, ways, relations))
}

#[cfg(test)]
mod tests {

    /// One host for this module's tests, so a run and the publication of
    /// its output share an invocation (publication refuses another). Never
    /// cancelled and never armed.
    fn run_host() -> &'static crate::host::Host {
        static HOST: std::sync::OnceLock<crate::host::Host> = std::sync::OnceLock::new();
        HOST.get_or_init(crate::host::Host::for_tests)
    }
    use super::{
        FULL_SCAN_ARM_MIN_BLOBS, GetparentsOptions, ScanArm, getparents_dispatched,
        getparents_with_arm,
    };
    use crate::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
    use crate::commands::getid::parse_ids;
    use crate::write::WriteOptions;
    use crate::writer::{Compression, PbfWriter};
    use crate::{Element, ElementReader, HeaderOverrides, MemberId};

    /// `path` pinned, for driving an arm directly.
    fn pinned(path: &std::path::Path) -> crate::read::input::PinnedInput {
        crate::read::input::PinnedInput::pin(path, run_host()).expect("pin input")
    }

    /// Write a node block, then optionally a way block, which can be written
    /// without indexdata to model a partially indexed input.
    fn write_blocks(
        path: &std::path::Path,
        node_ids: &[i64],
        way: Option<(i64, &[i64])>,
        index_way_blob: bool,
    ) {
        let file = std::fs::File::create(path).expect("create fixture");
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::default(),
        );
        writer
            .write_header(&HeaderBuilder::new().sorted().build().expect("header"))
            .expect("write header");
        let mut block = BlockBuilder::new();
        for &id in node_ids {
            block.add_node(id, 0, 0, std::iter::empty::<(&str, &str)>(), None);
        }
        writer
            .write_primitive_block(block.take().expect("node block").expect("nodes"))
            .expect("write nodes");
        if let Some((way_id, refs)) = way {
            block.add_way(way_id, std::iter::empty::<(&str, &str)>(), refs, None);
            let bytes = block.take().expect("way block").expect("ways");
            if index_way_blob {
                writer.write_primitive_block(bytes).expect("write ways");
            } else {
                writer
                    .write_primitive_block_no_indexdata(bytes)
                    .expect("write ways");
            }
        }
        writer.flush().expect("flush fixture");
    }

    fn fixture(path: &std::path::Path) {
        write_blocks(path, &[1, 2], Some((10, &[1, 2])), true);
    }

    fn ids(path: &std::path::Path) -> Vec<i64> {
        let mut ids = Vec::new();
        ElementReader::open_in(path, false, run_host())
            .expect("read output")
            .for_each(|element| match element {
                Element::DenseNode(node) => ids.push(node.id()),
                Element::Node(node) => ids.push(node.id()),
                Element::Way(way) => ids.push(way.id()),
                Element::Relation(relation) => ids.push(relation.id()),
            })
            .expect("iterate output");
        ids.sort_unstable();
        ids
    }

    #[test]
    fn full_scan_writes_parents() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        fixture(&input);
        let query = parse_ids(&["n1".to_owned()]).expect("ids");
        let opts = GetparentsOptions { add_self: true };
        let output = dir.path().join("output.pbf");
        let staged = crate::output::Output::new(
            &output,
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )
        .expect("stage output");
        getparents_with_arm(
            &pinned(&input),
            &staged,
            &query,
            &opts,
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            ScanArm::FullScan,
            run_host(),
        )
        .expect("getparents");
        staged.publish(&run_host().invocation).expect("publish");
        assert_eq!(ids(&output), vec![1, 10]);
    }

    /// Run the same query under both arms, assert they emit identical element
    /// sets, and return that common (sorted) ID list.
    fn ids_from_both_arms(input: &std::path::Path, query: &[&str], add_self: bool) -> Vec<i64> {
        let dir = crate::path_guard::tests::scratch();
        let query: Vec<String> = query.iter().map(|s| (*s).to_owned()).collect();
        let query = parse_ids(&query).expect("ids");
        let opts = GetparentsOptions { add_self };
        let walker = dir.path().join("walker.pbf");
        let pipelined = dir.path().join("pipelined.pbf");
        for (output, arm) in [(&walker, ScanArm::Walker), (&pipelined, ScanArm::FullScan)] {
            let staged = crate::output::Output::new(
                output,
                crate::output::Overwrite::Refuse,
                &crate::host::Diagnostics::discard(),
            )
            .expect("stage output");
            getparents_with_arm(
                &pinned(input),
                &staged,
                &query,
                &opts,
                &WriteOptions::default(),
                &HeaderOverrides::default(),
                arm,
                run_host(),
            )
            .expect("getparents");
            staged.publish(&run_host().invocation).expect("publish");
        }
        let walker_ids = ids(&walker);
        assert_eq!(walker_ids, ids(&pipelined));
        walker_ids
    }

    #[test]
    fn walker_and_pipelined_arms_emit_the_same_elements() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        fixture(&input);
        // Plain parent query: the referencing way, not the queried node.
        assert_eq!(ids_from_both_arms(&input, &["n1"], false), vec![10]);
        // --add-self additionally emits the queried node.
        assert_eq!(ids_from_both_arms(&input, &["n1"], true), vec![1, 10]);
    }

    #[test]
    fn arms_agree_on_empty_query_result() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        fixture(&input);
        assert!(ids_from_both_arms(&input, &["n999"], true).is_empty());
    }

    #[test]
    fn arms_agree_on_single_data_blob_file() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_blocks(&input, &[1, 2], None, true);
        assert_eq!(ids_from_both_arms(&input, &["n1"], true), vec![1]);
    }

    /// Fixture exercising cross-kind blob skipping. Each kind lands in its
    /// own indexed blob so the walker's `needed_blob_kinds` skip applies per
    /// kind, and the relation-of-relation pins single-level parent semantics:
    ///   node blob:     nodes 1, 2
    ///   way blob:      way 10 -> refs [1, 2]
    ///   relation blob: rel 100 -> [node 1, way 10]; rel 101 -> [rel 100]
    fn write_rich_fixture(path: &std::path::Path) {
        write_rich_fixture_indexed(path, [true; 3]);
    }

    /// [`write_rich_fixture`] with indexdata on the node, way and relation
    /// blob only where `indexed` says so, to model a partially indexed input.
    fn write_rich_fixture_indexed(path: &std::path::Path, indexed: [bool; 3]) {
        fn write(
            writer: &mut PbfWriter<std::io::BufWriter<std::fs::File>>,
            bytes: &[u8],
            indexed: bool,
        ) {
            if indexed {
                writer.write_primitive_block(bytes).expect("write block");
            } else {
                writer
                    .write_primitive_block_no_indexdata(bytes)
                    .expect("write block");
            }
        }
        let file = std::fs::File::create(path).expect("create fixture");
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::default(),
        );
        writer
            .write_header(&HeaderBuilder::new().sorted().build().expect("header"))
            .expect("write header");
        // Populated metadata, so a byte comparison of outputs covers it too.
        let meta = crate::block_builder::Metadata {
            version: 3,
            timestamp: 1_600_000_000,
            changeset: 42,
            uid: 7,
            user: "u",
            visible: true,
        };
        let meta = Some(&meta);
        let mut block = BlockBuilder::new();
        for id in [1i64, 2] {
            block.add_node(id, 0, 0, [("amenity", "cafe")], meta);
        }
        write(
            &mut writer,
            block.take().expect("node block").expect("nodes"),
            indexed[0],
        );
        block.add_way(10, [("highway", "path")], &[1, 2], meta);
        write(
            &mut writer,
            block.take().expect("way block").expect("way"),
            indexed[1],
        );
        let rel100 = [
            MemberData {
                id: MemberId::Node(1),
                role: "",
            },
            MemberData {
                id: MemberId::Way(10),
                role: "",
            },
        ];
        block.add_relation(100, [("type", "route")], &rel100, meta);
        let rel101 = [MemberData {
            id: MemberId::Relation(100),
            role: "",
        }];
        block.add_relation(101, std::iter::empty::<(&str, &str)>(), &rel101, meta);
        write(
            &mut writer,
            block.take().expect("relation block").expect("relations"),
            indexed[2],
        );
        writer.flush().expect("flush fixture");
    }

    /// The output's elements as (kind, id), in file order.
    fn ordered(path: &std::path::Path) -> Vec<(char, i64)> {
        let mut out = Vec::new();
        ElementReader::open_in(path, false, run_host())
            .expect("read output")
            .for_each(|element| match element {
                Element::DenseNode(node) => out.push(('n', node.id())),
                Element::Node(node) => out.push(('n', node.id())),
                Element::Way(way) => out.push(('w', way.id())),
                Element::Relation(relation) => out.push(('r', relation.id())),
            })
            .expect("iterate output");
        out
    }

    /// One arm's run: output bytes, ordered elements and stats.
    struct ArmRun {
        bytes: Vec<u8>,
        elements: Vec<(char, i64)>,
        stats: (u64, u64, u64),
    }

    /// A query, whether `--add-self` is set, and the expected output.
    struct Case {
        query: &'static [&'static str],
        add_self: bool,
        expected: &'static [(char, i64)],
    }

    #[expect(
        clippy::unwrap_in_result,
        reason = "test helper: the command's result is what is under test, the fixture I/O \
                  around it is not"
    )]
    fn run_arm(
        input: &std::path::Path,
        query: &[&str],
        add_self: bool,
        arm: ScanArm,
    ) -> crate::Result<ArmRun> {
        let dir = crate::path_guard::tests::scratch();
        let query: Vec<String> = query.iter().map(|s| (*s).to_owned()).collect();
        let query = parse_ids(&query).expect("ids");
        let output = dir.path().join("out.pbf");
        let staged = crate::output::Output::new(
            &output,
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )
        .expect("stage output");
        let stats = getparents_with_arm(
            &pinned(input),
            &staged,
            &query,
            &GetparentsOptions { add_self },
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            arm,
            run_host(),
        )?;
        staged.publish(&run_host().invocation).expect("publish");
        Ok(ArmRun {
            bytes: std::fs::read(&output).expect("read output bytes"),
            elements: ordered(&output),
            stats: (
                stats.nodes_written,
                stats.ways_written,
                stats.relations_written,
            ),
        })
    }

    /// Both arms give every case's output byte for byte alike, with the
    /// same stats: the queried node itself (`--add-self`), a way parent, a
    /// relation parent, a relation-of-relation parent. (A partially indexed
    /// input is refused on both arms mid-write, which this crate's test
    /// builds assert on as a writer dropped unflushed; the CLI input-contract
    /// table pins that refusal, in both orders.)
    #[test]
    fn arms_agree_byte_for_byte_on_every_parent_kind() {
        let dir = crate::path_guard::tests::scratch();
        let whole = dir.path().join("whole.pbf");
        write_rich_fixture_indexed(&whole, [true; 3]);
        let cases = [
            Case {
                query: &["n1"],
                add_self: false,
                expected: &[('w', 10), ('r', 100)],
            },
            Case {
                query: &["n1"],
                add_self: true,
                expected: &[('n', 1), ('w', 10), ('r', 100)],
            },
            Case {
                query: &["w10"],
                add_self: false,
                expected: &[('r', 100)],
            },
            Case {
                query: &["r100"],
                add_self: false,
                expected: &[('r', 101)],
            },
        ];
        for case in &cases {
            let walker =
                run_arm(&whole, case.query, case.add_self, ScanArm::Walker).expect("walker");
            let full =
                run_arm(&whole, case.query, case.add_self, ScanArm::FullScan).expect("full scan");
            let what = format!("{:?}, add_self {}", case.query, case.add_self);
            assert_eq!(walker.elements, case.expected, "{what}");
            let count = |kind| {
                let n = case.expected.iter().filter(|(k, _)| *k == kind).count();
                u64::try_from(n).expect("count fits")
            };
            assert_eq!(
                walker.stats,
                (count('n'), count('w'), count('r')),
                "stats: {what}"
            );
            assert_eq!(full.stats, walker.stats, "full-scan stats: {what}");
            assert!(full.bytes == walker.bytes, "arm outputs differ: {what}");
        }
    }

    // The four tests below assert against hand-computed parent sets, NOT just
    // walker-vs-pipelined agreement: both arms share `needed_blob_kinds`, so a
    // wrong kind-mapping would drop the same parent in both and cross-arm
    // equality alone would still pass. The expected literals are the oracle.

    /// A node query must scan relation blobs: relations can hold a node member
    /// directly. Confirms node blobs are skipped without dropping the relation
    /// parent, and that single-level semantics exclude rel 101 (which only
    /// references rel 100, not the queried node).
    #[test]
    fn node_query_scans_relation_blobs_for_relation_parents() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_rich_fixture(&input);
        assert_eq!(ids_from_both_arms(&input, &["n1"], false), vec![10, 100]);
    }

    /// A way query skips way blobs (nothing references a way from a way) but
    /// must still scan relation blobs for the relation that holds way 10 as a
    /// member. Without --add-self the queried way itself is not emitted.
    #[test]
    fn way_query_skips_way_blobs_but_finds_relation_parents() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_rich_fixture(&input);
        assert_eq!(ids_from_both_arms(&input, &["w10"], false), vec![100]);
    }

    /// A relation query skips node and way blobs (neither can reference a
    /// relation) and finds only the relation-of-relation parent.
    #[test]
    fn relation_query_scans_relation_blobs_only() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_rich_fixture(&input);
        assert_eq!(ids_from_both_arms(&input, &["r100"], false), vec![101]);
    }

    /// --add-self on a node query flips node blobs back on: the queried node
    /// appears alongside its way and relation parents from the other blobs.
    #[test]
    fn add_self_node_query_emits_node_and_all_parent_kinds() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        write_rich_fixture(&input);
        assert_eq!(ids_from_both_arms(&input, &["n1"], true), vec![1, 10, 100]);
    }

    #[test]
    fn auto_dispatch_crosses_arms_at_the_injected_threshold() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("input.pbf");
        fixture(&input);
        let query = parse_ids(&["n1".to_owned()]).expect("ids");
        let opts = GetparentsOptions { add_self: true };
        for (min_blobs, expected_arm) in [
            (1, ScanArm::FullScan),
            (FULL_SCAN_ARM_MIN_BLOBS, ScanArm::Walker),
        ] {
            let output = dir.path().join(format!("out-{min_blobs}.pbf"));
            let staged = crate::output::Output::new(
                &output,
                crate::output::Overwrite::Refuse,
                &crate::host::Diagnostics::discard(),
            )
            .expect("stage output");
            let (arm, _) = getparents_dispatched(
                &input,
                &staged,
                &query,
                &opts,
                &WriteOptions::default(),
                &HeaderOverrides::default(),
                min_blobs,
                run_host(),
            )
            .expect("getparents");
            staged.publish(&run_host().invocation).expect("publish");
            assert_eq!(arm, expected_arm);
            assert_eq!(ids(&output), vec![1, 10]);
        }
    }
}
