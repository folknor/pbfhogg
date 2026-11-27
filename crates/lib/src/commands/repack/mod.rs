//! Re-encode a PBF with a configurable per-blob element cap.
//!
//! The decoded count of the current kind determines each input blob's
//! treatment. A shrink blob (at least the cap) produces full blocks and its
//! own remainder. A grow blob contributes slices to a run whose blocks may
//! span several inputs. The run ends before the next shrink blob or at the
//! end of the kind. Workers encode and frame both kinds of blocks.
//!
//! Indexed input runs in three phases. Unindexed input is decoded in file
//! order just once, routing each blob's actual kinds to the same packing
//! rules and staging later kinds' finished frames for type-ordered output.

mod encode;
mod partition;
mod spool;

use super::{HeaderOverrides, Result, require_indexdata, writer_from_header};
use crate::ElementKind;
use crate::ElementReader;
use crate::read::input::InputSource;
use crate::scan::classify::{KindClaims, KindSchedules};
use crate::write::WriteOptions;

const KINDS: [ElementKind; 3] = [ElementKind::Node, ElementKind::Way, ElementKind::Relation];
const PHASE_MARKERS: [(&str, &str); 3] = [
    ("REPACK_NODES_START", "REPACK_NODES_END"),
    ("REPACK_WAYS_START", "REPACK_WAYS_END"),
    ("REPACK_RELATIONS_START", "REPACK_RELATIONS_END"),
];

/// Per-run statistics from a repack operation.
pub struct RepackStats {
    pub blobs_written: u64,
    pub elements_written: u64,
    pub elements_per_blob: usize,
}

impl RepackStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "Repacked {} elements into {} blobs (cap {} elements/blob)",
            self.elements_written, self.blobs_written, self.elements_per_blob,
        )?;

        Ok(())
    }
}

/// Re-encode `input` to `output` with a per-blob element cap of
/// `elements_per_blob`. Element semantics are preserved: every element
/// round-trips with its tags, refs, members, metadata, and DenseNodes
/// encoding, at the OSM data model's precision: every output block is in
/// the default framing, so a source block at another `granularity`, offset
/// or `date_granularity` has its coordinates truncated to decimicrodegrees
/// and its timestamps rounded down to whole seconds (`owned.rs`
/// `output_timestamp_seconds`; docs/cli/commands.md "PBF rebuild
/// precision"). Output is type-sorted (nodes, then ways, then relations);
/// the `Sort.Type_then_ID` flag is propagated when the input has it.
///
/// The input is read in several passes, so it must be a regular file
/// (`InvalidArgument` otherwise), and is pinned for the run
/// (see [`PinnedInput`](crate::read::input::PinnedInput)); a caller's pin is
/// used as it is. Where a later open cannot reach the pinned file, an input
/// replaced mid-run fails with `InputReplaced` and nothing is published.
pub fn repack<'a>(
    input: impl Into<InputSource<'a>>,
    output: impl crate::output::BindOutput,
    elements_per_blob: std::num::NonZeroUsize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<RepackStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = repack_to(
        input.into(),
        &output,
        elements_per_blob,
        write,
        overrides,
        host,
    )?;
    output.publish()?;
    Ok(result)
}

/// [`repack`] writing to a borrowed output it does not publish.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn repack_to(
    input: InputSource<'_>,
    output: &crate::output::BoundOutput,
    cap: std::num::NonZeroUsize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<RepackStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        force,
    } = *write;
    let elements_per_blob = cap.get();

    // The input is opened three times, each through its own open file
    // description: the indexdata probe, the header reader, then the schedule
    // walk whose descriptor every per-kind pool preads. All three are derived
    // from this one pin, and the size bound below is read from it, so a
    // file renamed over the input mid-run cannot be probed, headed and
    // re-encoded partly from each file: where `/proc/self/fd` reopens the pin
    // every open reaches the pinned file, otherwise on Unix a replaced input
    // fails the run (`PinnedInput` says what it does off Unix). Taken before
    // any other open of the input.
    let pin = input.pin(host)?;
    let input = &pin;

    require_indexdata(
        input,
        direct_io,
        force,
        "input PBF has no blob-level indexdata. repack uses the parallel \
         per-kind classify pipeline, which needs indexdata to build per-kind \
         blob schedules.",
        host,
    )?;

    // Unlike tags-filter / extract, repack round-trips way payloads with
    // their inline coordinates, so an input declaring LocationsOnWays keeps
    // it: the workers thread the coordinates through both packing paths.
    // With no LocationsOnWays declaration they build plain ways. The
    // prepass fields are not rewritten, so their declarations go, with
    // the shared warning.
    let (header, preserve_locations) = {
        let reader = ElementReader::open_in(input, direct_io, host)?;
        let preserve = reader.header().has_locations_on_ways();
        (reader.header().clone(), preserve)
    };
    let mut writer = writer_from_header(
        output,
        compression,
        &header,
        &super::output_header::OutputFacts::carry(
            super::output_header::Ways::ReencodedWithLocations,
        ),
        overrides,
        backend,
        host,
    )?;

    let (schedules, file, _) =
        crate::scan::classify::build_classify_schedules_split(input, KindClaims::Refuse, host)?;
    // Bound every worker's initial allocation by the input's size. A
    // caller-supplied cap still controls packing and may be much larger.
    let config = encode::Config {
        cap,
        prealloc_bound: usize::try_from(input.metadata()?.len()).unwrap_or(usize::MAX),
        compression,
        preserve_locations,
        host,
        input_path: file.path(),
    };
    let phases = run_schedules(schedules, &file, &config, &mut writer, output.path())?;

    host.invocation.marker("REPACK_FLUSH_START");
    writer.flush()?;
    host.invocation.marker("REPACK_FLUSH_END");

    Ok(summarize(&phases, elements_per_blob, host))
}

fn run_schedules(
    schedules: KindSchedules,
    file: &crate::read::input::InputFile,
    config: &encode::Config<'_, '_>,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
    staging_path: &std::path::Path,
) -> Result<[PhaseStats; 3]> {
    let invocation = &config.host.invocation;
    let mut phases = [PhaseStats::default(); 3];
    match schedules {
        KindSchedules::Indexed {
            nodes,
            ways,
            relations,
        } => {
            let schedules = [nodes, ways, relations];
            emit_input_counters(schedules.each_ref().map(Vec::len), invocation);
            let mut output = spool::FrameOutput::new(writer, None, config.host);
            for ((kind, schedule), (start, end)) in
                KINDS.into_iter().zip(schedules).zip(PHASE_MARKERS)
            {
                invocation.marker(start);
                let mut plan = partition::Planner::new(config, &mut output);
                plan.scan(file, &schedule)?;
                let index = usize::from(kind.ordinal());
                phases[index] = plan.finish()?[index];
                invocation.marker(end);
                emit_phase_counters(kind, &phases[index], invocation);
            }
        }
        KindSchedules::Unindexed(schedule) => {
            emit_input_counters([schedule.len(); 3], invocation);
            let parent = staging_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let mut output = spool::FrameOutput::new(writer, Some(parent), config.host);
            invocation.marker("REPACK_UNINDEXED_START");
            invocation.marker(PHASE_MARKERS[0].0);
            let mut plan = partition::Planner::new(config, &mut output);
            plan.scan(file, &schedule)?;
            phases = plan.finish()?;
            invocation.marker(PHASE_MARKERS[0].1);
            invocation.marker("REPACK_UNINDEXED_END");
            emit_phase_counters(ElementKind::Node, &phases[0], invocation);
            for kind in [ElementKind::Way, ElementKind::Relation] {
                let index = usize::from(kind.ordinal());
                let (start, end) = PHASE_MARKERS[index];
                invocation.marker(start);
                output.replay(kind)?;
                invocation.marker(end);
                emit_phase_counters(kind, &phases[index], invocation);
            }
        }
    }
    Ok(phases)
}

fn emit_input_counters(counts: [usize; 3], invocation: &crate::host::Recorder) {
    // For unindexed input these are the candidate counts for each kind.
    invocation.counter("repack_input_blobs_nodes", counts[0]);
    invocation.counter("repack_input_blobs_ways", counts[1]);
    invocation.counter("repack_input_blobs_relations", counts[2]);
    invocation.counter(
        "repack_input_blobs_total",
        counts.into_iter().sum::<usize>(),
    );
}

fn summarize(
    phases: &[PhaseStats; 3],
    elements_per_blob: usize,
    host: &crate::host::Entered<'_>,
) -> RepackStats {
    let blobs_written = phases.iter().map(PhaseStats::blobs).sum::<u64>();
    let elements_written = phases.iter().map(|s| s.elements).sum::<u64>();
    let any_cap_fired = phases.iter().any(|s| s.blobs() > 1);
    let invocation = &host.invocation;
    invocation.counter("repack_blobs_written", blobs_written);
    invocation.counter("repack_elements_written", elements_written);
    for (name, count) in [
        (
            "repack_input_blobs_coalesced",
            phases.iter().map(|s| s.coalesces).sum::<u64>(),
        ),
        (
            "repack_worker_full_blobs",
            phases.iter().map(|s| s.worker_full_blobs).sum::<u64>(),
        ),
        (
            "repack_worker_tail_blobs",
            phases.iter().map(|s| s.worker_tail_blobs).sum::<u64>(),
        ),
        (
            "repack_central_blobs",
            phases.iter().map(|s| s.grow_blobs).sum::<u64>(),
        ),
    ] {
        invocation.counter(name, count);
    }
    invocation.counter("repack_cap_fired", any_cap_fired);
    invocation.counter("repack_elements_per_blob", elements_per_blob);

    if !any_cap_fired && elements_written > 0 {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "repack.cap_never_fired",
            "the element cap of {elements_per_blob} per blob never fired; \
             every per-kind element count fits in a single output blob, \
             so the output is one blob per kind."
        );
    }

    RepackStats {
        blobs_written,
        elements_written,
        elements_per_blob,
    }
}

#[derive(Clone, Copy, Default)]
struct PhaseStats {
    elements: u64,
    coalesces: u64,
    worker_full_blobs: u64,
    worker_tail_blobs: u64,
    /// Grow partitions. The sidecar still exposes these as central_blobs.
    grow_blobs: u64,
}

impl PhaseStats {
    fn blobs(&self) -> u64 {
        self.worker_full_blobs + self.worker_tail_blobs + self.grow_blobs
    }
}

/// Sidecar counter names for one kind's phase, in emission order: blobs,
/// elements, coalesces, worker_full_blobs, central_blobs, cap_fired,
/// worker_tail_blobs. Spelled out literally so each name can be grepped
/// verbatim.
fn phase_counter_names(kind: ElementKind) -> [&'static str; 7] {
    match kind {
        ElementKind::Node => [
            "repack_nodes_blobs",
            "repack_nodes_elements",
            "repack_nodes_coalesces",
            "repack_nodes_worker_full_blobs",
            "repack_nodes_central_blobs",
            "repack_nodes_cap_fired",
            "repack_nodes_worker_tail_blobs",
        ],
        ElementKind::Way => [
            "repack_ways_blobs",
            "repack_ways_elements",
            "repack_ways_coalesces",
            "repack_ways_worker_full_blobs",
            "repack_ways_central_blobs",
            "repack_ways_cap_fired",
            "repack_ways_worker_tail_blobs",
        ],
        ElementKind::Relation => [
            "repack_relations_blobs",
            "repack_relations_elements",
            "repack_relations_coalesces",
            "repack_relations_worker_full_blobs",
            "repack_relations_central_blobs",
            "repack_relations_cap_fired",
            "repack_relations_worker_tail_blobs",
        ],
    }
}

/// Emit sidecar counters for one kind.
fn emit_phase_counters(kind: ElementKind, s: &PhaseStats, invocation: &crate::host::Recorder) {
    let [
        blobs,
        elements,
        coalesces,
        worker_full,
        central,
        cap_fired,
        worker_tail,
    ] = phase_counter_names(kind);
    invocation.counter(blobs, s.blobs());
    invocation.counter(elements, s.elements);
    invocation.counter(coalesces, s.coalesces);
    invocation.counter(worker_full, s.worker_full_blobs);
    invocation.counter(central, s.grow_blobs);
    invocation.counter(cap_fired, s.blobs() > 1);
    invocation.counter(worker_tail, s.worker_tail_blobs);
}

#[cfg(test)]
mod counter_name_tests {
    use super::*;

    #[test]
    fn phase_counter_names_match_the_old_format_spelling() {
        for (kind, word) in [
            (ElementKind::Node, "nodes"),
            (ElementKind::Way, "ways"),
            (ElementKind::Relation, "relations"),
        ] {
            let want = [
                "blobs",
                "elements",
                "coalesces",
                "worker_full_blobs",
                "central_blobs",
                "cap_fired",
                "worker_tail_blobs",
            ]
            .map(|c| format!("repack_{word}_{c}"));
            assert_eq!(phase_counter_names(kind).map(String::from), want);
        }
    }
}

#[cfg(test)]
mod byte_tests {
    use std::num::NonZeroUsize;
    use std::path::Path;

    use super::*;
    use crate::blob_meta::BlobIndex;
    use crate::block_builder::{BlockBuilder, HeaderBuilder, OwnedBlock};
    use crate::commands::flush_local;
    use crate::host::Host;
    use crate::output::{Output, Overwrite};
    use crate::owned::{
        OwnedWay, read_dense_node, read_node, read_relation, read_way, write_single_node_local,
        write_single_relation_local, write_single_way_local,
    };
    use crate::read::wire_fixtures::{
        Group, block, deltas, dense, field_bytes, field_varint, packed, plain_node, relation, way,
    };
    use crate::writer::{Compression, PbfWriter, ZlibLevel};
    use crate::{Element, PrimitiveBlock, Way};

    const SHAPES: [[usize; 3]; 8] = [
        [2, 0, 4],
        [1, 4, 0],
        [4, 2, 9],
        [2, 9, 0],
        [9, 2, 2],
        [2, 1, 4],
        [4, 0, 2],
        [1, 2, 0],
    ];

    fn fixture_block(first: [i64; 3], counts: [usize; 3], odd: bool) -> Vec<u8> {
        let (key, value, user, role) = if odd { (4, 3, 2, 1) } else { (1, 2, 3, 4) };
        let strings = if odd {
            ["", "outer", "mapper", "value", "key", "key"]
        } else {
            ["", "key", "value", "mapper", "outer", "key"]
        };
        let dense_count = counts[0] / 2;
        let dense_nodes: Vec<_> = (0..dense_count)
            .map(|i| {
                let id = first[0] + i64::try_from(i).expect("fixture id");
                (id, id * 19, -id * 31)
            })
            .collect();
        let group = Group {
            dense: (!dense_nodes.is_empty()).then(|| {
                let mut data = dense(&dense_nodes, &[key, value, 0].repeat(dense_count));
                let mut info = Vec::new();
                // Partial dense columns, including a username and a timestamp
                // in the block's non-default date granularity.
                field_bytes(&mut info, 1, &packed((0..dense_count).map(|_| 2)));
                field_bytes(&mut info, 2, &deltas(&vec![37; dense_count]));
                field_bytes(&mut info, 5, &deltas(&vec![user; dense_count]));
                field_bytes(&mut data, 5, &info);
                data
            }),
            nodes: (dense_count..counts[0])
                .map(|i| {
                    let id = first[0] + i64::try_from(i).expect("fixture id");
                    let info = (i % 2 == 0).then_some([0, 9, 0, 0, user]);
                    plain_node(id, id * 7, -id * 17, &[(5, value)], info)
                })
                .collect(),
            ways: (0..counts[1])
                .map(|i| {
                    let id = first[1] + i64::try_from(i).expect("fixture id");
                    let mut data = way(id, &[1, 3, -5], (key, value));
                    if i % 2 == 0 {
                        field_bytes(&mut data, 9, &deltas(&[11, 33, -55]));
                        field_bytes(&mut data, 10, &deltas(&[31, -53, 72]));
                    }
                    let mut info = Vec::new();
                    field_varint(&mut info, 2, 37);
                    field_varint(&mut info, 5, u64::try_from(user).expect("fixture sid"));
                    field_bytes(&mut data, 4, &info);
                    data
                })
                .collect(),
            relations: (0..counts[2])
                .map(|i| {
                    let id = first[2] + i64::try_from(i).expect("fixture id");
                    let mut data = relation(id, &[(role, 3, 0), (key, 4, 1), (0, 9, 2)]);
                    field_bytes(&mut data, 2, &packed([key]));
                    field_bytes(&mut data, 3, &packed([value]));
                    data
                })
                .collect(),
        };
        let mut data = block(&strings, &[group]);
        field_varint(&mut data, 17, 10);
        field_varint(&mut data, 18, 150);
        data
    }

    fn fixture(indexed: bool) -> Vec<(Option<ElementKind>, Vec<u8>)> {
        let mut out = Vec::new();
        let mut first = [1, 101, 201];
        for (i, counts) in SHAPES.into_iter().enumerate() {
            if indexed {
                for kind in KINDS {
                    let index = usize::from(kind.ordinal());
                    if counts[index] != 0 {
                        let mut only = [0; 3];
                        only[index] = counts[index];
                        out.push((Some(kind), fixture_block(first, only, i % 2 != 0)));
                    }
                }
            } else {
                out.push((None, fixture_block(first, counts, i % 2 != 0)));
            }
            for (id, count) in first.iter_mut().zip(counts) {
                *id += i64::try_from(count).expect("fixture count");
            }
        }
        out
    }

    fn write_fixture(
        path: &Path,
        blocks: &[(Option<ElementKind>, Vec<u8>)],
        locations: bool,
        host: &Host,
    ) -> Result<()> {
        let mut header = HeaderBuilder::new();
        if locations {
            header = header.optional_feature(crate::HeaderBlock::LOCATIONS_ON_WAYS);
        }
        let compression = Compression::Zlib(ZlibLevel::FAST);
        let mut writer = PbfWriter::new(std::fs::File::create(path)?, compression);
        writer.write_header(&header.build()?)?;
        let host = host.entered();
        for (kind, data) in blocks {
            if let Some(kind) = kind {
                // The claim names the right kind but supplies a deliberately
                // inaccurate count. The planner must use the decoded count.
                let index = BlobIndex {
                    kind: *kind,
                    min_id: 1,
                    max_id: 500,
                    count: 1,
                    bbox: None,
                };
                let frame = crate::writer::frame_blob_pipelined(
                    data,
                    &compression,
                    Some(&index.serialize()),
                    None,
                    None,
                    &host.invocation.metrics().writer,
                )?;
                writer.write_raw_owned(frame)?;
            } else {
                writer.write_primitive_block_no_indexdata(data)?;
            }
        }
        writer.flush()
    }

    /// The old repack grow path captured inline locations when the header declared them.
    fn read_way_with_locations(way: &Way<'_>) -> Result<OwnedWay> {
        let mut owned = read_way(way)?;
        owned.locations = way
            .node_locations()
            .map(|loc| Ok((loc.decimicro_lat()?, loc.decimicro_lon()?)))
            .collect::<Result<_>>()?;
        Ok(owned)
    }

    fn old_write(
        writer: &mut PbfWriter<crate::file_writer::FileWriter>,
        blocks: &[PrimitiveBlock],
        kind: ElementKind,
        cap: NonZeroUsize,
        locations: bool,
    ) -> Result<()> {
        let mut bb = BlockBuilder::with_element_cap_prealloc(cap, cap.get());
        let mut framed = Vec::<OwnedBlock>::new();
        for block in blocks {
            let elements: Vec<_> = block
                .elements()
                .filter(|element| match element {
                    Element::DenseNode(_) | Element::Node(_) => kind == ElementKind::Node,
                    Element::Way(_) => kind == ElementKind::Way,
                    Element::Relation(_) => kind == ElementKind::Relation,
                })
                .collect();
            let shrink = elements.len() >= cap.get();
            if shrink {
                flush_local(&mut bb, &mut framed)?;
            }
            for element in elements {
                match element {
                    Element::DenseNode(n) => {
                        write_single_node_local(&read_dense_node(&n)?, &mut bb, &mut framed)?;
                    }
                    Element::Node(n) => {
                        write_single_node_local(&read_node(&n)?, &mut bb, &mut framed)?;
                    }
                    Element::Way(w) => {
                        let way = if locations {
                            read_way_with_locations(&w)?
                        } else {
                            read_way(&w)?
                        };
                        write_single_way_local(&way, &mut bb, &mut framed)?;
                    }
                    Element::Relation(r) => {
                        write_single_relation_local(&read_relation(&r)?, &mut bb, &mut framed)?;
                    }
                }
            }
            if shrink {
                flush_local(&mut bb, &mut framed)?;
            }
            for output in framed.drain(..) {
                writer.write_primitive_block_owned(output)?;
            }
        }
        flush_local(&mut bb, &mut framed)?;
        for output in framed {
            writer.write_primitive_block_owned(output)?;
        }
        Ok(())
    }

    fn byte_comparison(indexed: bool, locations: bool, compression: Compression) -> Result<()> {
        let directory = pbfhogg_test_scratch::scratch();
        let input = directory.path().join("input.osm.pbf");
        let actual = directory.path().join("actual.osm.pbf");
        let expected = directory.path().join("expected.osm.pbf");
        let host = Host::for_tests();
        let blocks = fixture(indexed);
        write_fixture(&input, &blocks, locations, &host)?;
        let write = WriteOptions {
            compression,
            force: !indexed,
            ..WriteOptions::default()
        };
        let overrides = HeaderOverrides::default();
        let cap = NonZeroUsize::MIN.saturating_add(4);
        let stats = repack(
            input.as_path(),
            Output::new(&actual, Overwrite::Refuse, &host.diagnostics)?,
            cap,
            &write,
            &overrides,
            &host,
        )?;
        assert_eq!(
            stats.elements_written,
            SHAPES.into_iter().flatten().sum::<usize>() as u64
        );

        let entered = host.entered();
        let header = ElementReader::open_in(input.as_path(), false, &entered)?
            .header()
            .clone();
        let target =
            Output::new(&expected, Overwrite::Refuse, &host.diagnostics)?.bind(&host.invocation)?;
        let mut writer = writer_from_header(
            &target,
            compression,
            &header,
            &crate::commands::output_header::OutputFacts::carry(
                crate::commands::output_header::Ways::ReencodedWithLocations,
            ),
            &overrides,
            write.backend,
            &entered,
        )?;
        let blocks: Vec<_> = blocks
            .into_iter()
            .map(|(_, bytes)| PrimitiveBlock::from_vec(bytes))
            .collect::<Result<_>>()?;
        for kind in KINDS {
            old_write(&mut writer, &blocks, kind, cap, locations)?;
        }
        writer.flush()?;
        drop(writer);
        target.publish()?;
        assert_eq!(std::fs::read(actual)?, std::fs::read(expected)?);
        Ok(())
    }

    #[test]
    fn indexed_grow_preserves_legacy_bytes_across_mixed_sizes_and_shrink_runs() -> Result<()> {
        byte_comparison(true, true, Compression::Zlib(ZlibLevel::FAST))
    }

    #[test]
    fn forced_mixed_kind_blobs_preserve_legacy_bytes_with_and_without_locations() -> Result<()> {
        let Ok(compression) = Compression::zstd(1) else {
            panic!("zstd level 1 is valid");
        };
        byte_comparison(false, true, compression)?;
        byte_comparison(false, false, Compression::None)
    }
}
