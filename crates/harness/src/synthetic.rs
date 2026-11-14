//! The synthetic read and write workloads: count a PBF's elements through
//! one of the library's public readers, or decode it and re-encode every
//! element through `BlockBuilder` and a `PbfWriter` into a sink. They exist
//! to be measured (the `decode-*` and `encode-*` scenarios); the plan in
//! `workload.rs` spells every choice made here.
//!
//! Both read through the public constructors on purpose: they measure the
//! readers a library user gets, so, like a library user, they read a
//! partially indexed file the commands would refuse.

use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData};
use pbfhogg::host::Host;
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element, ElementReader};
use serde_json::{Value, json};
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

/// The discard sink every encode call writes into: a character device, no
/// role and no publication (`reference/library-protocol.md`, Discard sink).
pub(crate) const DISCARD: &str = "/dev/null";

/// The sync writer's buffer over the sink.
pub(crate) const SYNC_SINK_BUFFER: usize = 256 * 1024;

/// Element counts by type; dense and plain nodes both count as nodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub(crate) nodes: u64,
    pub(crate) ways: u64,
    pub(crate) relations: u64,
}

impl Counts {
    fn add(&mut self, element: &Element<'_>) {
        match element {
            Element::Node(_) | Element::DenseNode(_) => self.nodes += 1,
            Element::Way(_) => self.ways += 1,
            Element::Relation(_) => self.relations += 1,
            _ => {}
        }
    }

    fn of(element: &Element<'_>) -> Self {
        let mut counts = Self::default();
        counts.add(element);
        counts
    }

    fn sum(self, other: Self) -> Self {
        Self {
            nodes: self.nodes + other.nodes,
            ways: self.ways + other.ways,
            relations: self.relations + other.relations,
        }
    }
}

/// Which public reader a decode call counts through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reader {
    /// `ElementReader::for_each`.
    Sequential,
    /// `ElementReader::par_map_reduce`.
    Parallel,
    /// `ElementReader::for_each_pipelined`.
    Pipelined,
    /// `BlobReader`, each blob decoded on the calling thread.
    Blobreader,
}

impl Reader {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Parallel => "parallel",
            Self::Pipelined => "pipelined",
            Self::Blobreader => "blobreader",
        }
    }
}

/// Count `path`'s elements through `reader`, under `host` (whose page-cache
/// policy the plan sets).
pub(crate) fn decode(path: &Path, reader: Reader, host: &Host) -> pbfhogg::Result<Counts> {
    match reader {
        Reader::Sequential => {
            let mut counts = Counts::default();
            ElementReader::open_with(path, false, host)?.for_each(|el| counts.add(&el))?;
            Ok(counts)
        }
        Reader::Parallel => ElementReader::open_with(path, false, host)?.par_map_reduce(
            |el| Counts::of(&el),
            Counts::default,
            Counts::sum,
        ),
        Reader::Pipelined => {
            let mut counts = Counts::default();
            ElementReader::open_with(path, false, host)?
                .for_each_pipelined(|el| counts.add(&el))?;
            Ok(counts)
        }
        Reader::Blobreader => {
            // The library's buffered path (its 256 KiB FileReader), so this
            // arm measures decode architecture, not small-read syscalls.
            let mut blobs = BlobReader::open_with(path, false, host)?;
            let source: Arc<Path> = Arc::from(path);
            // The leading OsmHeader is required like everywhere else: a
            // header-less file is the missing-header error, not an empty run.
            let header = blobs.leading_header()?;
            header
                .decode()
                .map_err(|e| header.locate_error(Some(&source), e))?;
            let mut counts = Counts::default();
            for blob in blobs {
                let blob = blob?;
                let decoded = blob
                    .decode()
                    .map_err(|e| blob.locate_error(Some(&source), e))?;
                if let BlobDecode::OsmData(block) = decoded {
                    for el in block.elements() {
                        counts.add(&el);
                    }
                }
            }
            Ok(counts)
        }
    }
}

/// Which writer an encode call writes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Writer {
    /// `PbfWriter::new_in` over a [`SYNC_SINK_BUFFER`] `BufWriter`: framing
    /// and compression on the calling thread.
    Sync,
    /// `PbfWriter::to_path_with`: compression on the host's executor, a
    /// writer thread for the sink.
    Pipelined,
}

impl Writer {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Pipelined => "pipelined",
        }
    }
}

/// A decode call's plan entry: every choice [`decode`] makes.
pub(crate) fn decode_plan(input: &str, reader: Reader, host: &Value) -> Value {
    json!({
        "command": "decode",
        "input": input,
        "reader": reader.name(),
        "opened_with": match reader {
            Reader::Blobreader => "BlobReader::open_with, buffered",
            _ => "ElementReader::open_with, buffered",
        },
        "counts": "nodes (dense and plain), ways, relations",
        "host": host,
    })
}

/// An encode call's plan entry: every choice [`encode`] makes.
pub(crate) fn encode_plan(input: &str, writer: Writer, compression: &str, host: &Value) -> Value {
    json!({
        "command": "encode",
        "input": input,
        "input_reader": "BlobReader::new_named over a default-capacity BufReader",
        "encoded": "default header; node id, coordinates and tags; way id, tags and refs; relation id, tags and members with roles; no metadata, no way locations",
        "blocks": "BlockBuilder, cut where it fills, input order",
        "writer": match writer {
            Writer::Sync => json!({
                "kind": writer.name(),
                "constructor": "PbfWriter::new_in",
                "buffer_bytes": SYNC_SINK_BUFFER,
            }),
            Writer::Pipelined => json!({
                "kind": writer.name(),
                "constructor": "PbfWriter::to_path_with",
            }),
        },
        "compression": compression,
        "sink": "discard",
        "finish": "flush inside the call; a flush error is the call's",
        "host": host,
    })
}

/// Decode `input` and re-encode every element into `sink` through
/// `writer` with `compression`, under `host`. Production calls pass
/// [`DISCARD`]; tests pass a scratch file and decode it back.
///
/// What is encoded: a default header (no bbox, no sort flag), then each
/// node's id, coordinates and tags, each way's id, tags and refs, and each
/// relation's id, tags and members, roles included; no metadata and no
/// way locations. Blocks are cut where `BlockBuilder` fills, one element
/// type per block, in input order. The writer is flushed (and, pipelined,
/// its thread joined) before return, and a flush error is the call's.
pub(crate) fn encode(
    input: &Path,
    sink: &Path,
    writer: Writer,
    compression: Compression,
    host: &Host,
) -> pbfhogg::Result<Counts> {
    let header = HeaderBuilder::new().build()?;
    match writer {
        Writer::Sync => {
            let file = std::fs::File::create(sink)?;
            let mut out = PbfWriter::new_in(
                BufWriter::with_capacity(SYNC_SINK_BUFFER, file),
                compression,
                host.invocation.clone(),
            )?;
            out.write_header(&header)?;
            let counts = encode_elements(input, &mut out, host)?;
            out.flush()?;
            Ok(counts)
        }
        Writer::Pipelined => {
            let mut out = PbfWriter::to_path_with(sink, compression, &header, host)?;
            let counts = encode_elements(input, &mut out, host)?;
            // `flush`, not just drop: Drop joins the writer thread too but
            // discards its result and skips the completeness check.
            out.flush()?;
            Ok(counts)
        }
    }
}

/// The element loop both writers share.
fn encode_elements<W: Write>(
    input: &Path,
    out: &mut PbfWriter<W>,
    host: &Host,
) -> pbfhogg::Result<Counts> {
    let file = std::fs::File::open(input)?;
    let mut blobs = BlobReader::new_named(BufReader::new(file), input)
        .diagnostics(host.diagnostics.clone())
        .invocation(host.invocation.clone());
    let source: Arc<Path> = Arc::from(input);
    // Required and decoded first, so a corrupt header fails before any data.
    let header = blobs.leading_header()?;
    header
        .decode()
        .map_err(|e| header.locate_error(Some(&source), e))?;
    let mut bb = BlockBuilder::new();
    let mut counts = Counts::default();
    for blob in blobs {
        let blob = blob?;
        let decoded = blob
            .decode()
            .map_err(|e| blob.locate_error(Some(&source), e))?;
        let BlobDecode::OsmData(block) = decoded else {
            continue;
        };
        // Members borrow roles from this block, so the buffer lives per
        // block, as in the re-encoding commands.
        let mut members: Vec<MemberData<'_>> = Vec::new();
        for el in block.elements() {
            match &el {
                Element::DenseNode(n) => {
                    if !bb.can_add_node()
                        && let Some(bytes) = bb.take()?
                    {
                        out.write_primitive_block(bytes)?;
                    }
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), None);
                }
                Element::Node(n) => {
                    if !bb.can_add_node()
                        && let Some(bytes) = bb.take()?
                    {
                        out.write_primitive_block(bytes)?;
                    }
                    bb.add_node(n.id(), n.decimicro_lat(), n.decimicro_lon(), n.tags(), None);
                }
                Element::Way(w) => {
                    if !bb.can_add_way()
                        && let Some(bytes) = bb.take()?
                    {
                        out.write_primitive_block(bytes)?;
                    }
                    let refs: Vec<i64> = w.refs().collect();
                    bb.add_way(w.id(), w.tags(), &refs, None);
                }
                Element::Relation(r) => {
                    if !bb.can_add_relation()
                        && let Some(bytes) = bb.take()?
                    {
                        out.write_primitive_block(bytes)?;
                    }
                    // A bad role refuses the call: an empty role would encode
                    // a changed relation, and a revision whose role decoding
                    // fails must never read as faster.
                    r.members_into(&mut members)?;
                    bb.add_relation(r.id(), r.tags(), &members, None);
                }
                _ => {}
            }
            counts.add(&el);
        }
    }
    if let Some(bytes) = bb.take()? {
        out.write_primitive_block(bytes)?;
    }
    Ok(counts)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // fixtures in test scratch
mod tests {
    use super::{Counts, Reader, Writer, decode, encode};
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder, MemberData, Metadata};
    use pbfhogg::elements::MemberId;
    use pbfhogg::host::{Diagnostics, Executor, Fixed, Host, Invocation, NoProcessActions};
    use pbfhogg::writer::{Compression, PbfWriter};
    use pbfhogg::{Element, ElementReader};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// More nodes than one block holds, so blocks are cut mid-type.
    const NODES: i64 = 8_100;

    #[derive(Default)]
    struct Recording(Mutex<Vec<String>>);
    impl pbfhogg::host::DiagnosticSink for Recording {
        fn event(&self, event: &pbfhogg::host::Event<'_>) {
            self.0.lock().expect("sink").push(event.args.to_string());
        }
    }

    fn host(diagnostics: Diagnostics) -> Host {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("pool");
        Host {
            diagnostics,
            process: Arc::new(NoProcessActions),
            resources: Arc::new(Fixed {
                parallelism: std::num::NonZeroUsize::new(2),
                memory_available: None,
            }),
            page_cache: pbfhogg::host::PageCachePolicy::Keep,
            executor: Executor::Pool(Arc::new(pool)),
            invocation: Invocation::new(),
        }
    }

    fn plain_host() -> Host {
        host(Diagnostics::discard())
    }

    /// Nodes 1..=NODES (every tenth tagged, with metadata), three ways and
    /// two relations with mixed member types and roles.
    fn write_input(path: &Path) {
        let mut writer = PbfWriter::new(
            std::fs::File::create(path).expect("create"),
            Compression::Zlib(6),
        );
        writer
            .write_header(&HeaderBuilder::new().sorted().build().expect("header"))
            .expect("header");
        let meta = Metadata {
            version: 3,
            timestamp: 1_000,
            changeset: 7,
            uid: 9,
            user: "someone",
            visible: true,
        };
        let mut bb = BlockBuilder::new();
        let flush = |bb: &mut BlockBuilder, writer: &mut PbfWriter<std::fs::File>| {
            if let Some(bytes) = bb.take().expect("take") {
                writer.write_primitive_block(bytes).expect("block");
            }
        };
        for id in 1..=NODES {
            if !bb.can_add_node() {
                flush(&mut bb, &mut writer);
            }
            let lat = i32::try_from(id).expect("id") * 1_000;
            let tags: &[(&str, &str)] = if id % 10 == 0 {
                &[("amenity", "bench"), ("name", "n")]
            } else {
                &[]
            };
            bb.add_node(id, lat, -lat, tags.iter().copied(), Some(&meta));
        }
        flush(&mut bb, &mut writer);
        // Ways carry inline locations and metadata, both for encode to drop.
        for (id, refs) in [(1, vec![1, 2, 3]), (2, vec![3, 4]), (3, vec![5, 6, 7, 5])] {
            let locations: Vec<(i32, i32)> = refs
                .iter()
                .map(|&r| {
                    let lat = i32::try_from(r).expect("id") * 1_000;
                    (lat, -lat)
                })
                .collect();
            bb.add_way_with_locations(
                id,
                [("highway", "residential")],
                &refs,
                &locations,
                Some(&meta),
            );
        }
        flush(&mut bb, &mut writer);
        let members = [
            MemberData {
                id: MemberId::Way(1),
                role: "outer",
            },
            MemberData {
                id: MemberId::Node(4),
                role: "",
            },
            MemberData {
                id: MemberId::Relation(2),
                role: "subarea",
            },
        ];
        bb.add_relation(1, [("type", "multipolygon")], &members, Some(&meta));
        bb.add_relation(2, [("type", "route")], &members[..1], None);
        flush(&mut bb, &mut writer);
        writer.flush().expect("flush");
    }

    /// Every element as what encode keeps: ids, coordinates, tags, refs,
    /// members with roles. Metadata is not part of it.
    #[derive(Debug, PartialEq, Eq)]
    enum Kept {
        Node(i64, i32, i32, Vec<(String, String)>),
        Way(i64, Vec<(String, String)>, Vec<i64>),
        Relation(i64, Vec<(String, String)>, Vec<(MemberId, String)>),
    }

    /// What encode drops, as seen in a file: metadata on nodes, ways and
    /// relations, and inline way locations.
    #[derive(Debug, Default, PartialEq, Eq)]
    struct Droppable {
        node_metadata: bool,
        way_metadata: bool,
        relation_metadata: bool,
        way_locations: bool,
    }

    fn kept(path: &Path) -> (Vec<Kept>, Droppable) {
        let tags = |it: &mut dyn Iterator<Item = (&str, &str)>| {
            it.map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect::<Vec<_>>()
        };
        let mut out = Vec::new();
        let mut seen = Droppable::default();
        ElementReader::open_with(path, false, &plain_host())
            .expect("open")
            .for_each(|el| match el {
                Element::DenseNode(n) => {
                    seen.node_metadata |= n.info().is_some();
                    out.push(Kept::Node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        tags(&mut n.tags()),
                    ));
                }
                Element::Node(n) => {
                    seen.node_metadata |= n.info().version().is_some();
                    out.push(Kept::Node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        tags(&mut n.tags()),
                    ));
                }
                Element::Way(w) => {
                    seen.way_metadata |= w.info().version().is_some();
                    seen.way_locations |= w.node_locations().next().is_some();
                    out.push(Kept::Way(w.id(), tags(&mut w.tags()), w.refs().collect()));
                }
                Element::Relation(r) => {
                    seen.relation_metadata |= r.info().version().is_some();
                    let mut members = Vec::new();
                    r.members_into(&mut members).expect("roles");
                    out.push(Kept::Relation(
                        r.id(),
                        tags(&mut r.tags()),
                        members.iter().map(|m| (m.id, m.role.to_owned())).collect(),
                    ));
                }
                _ => {}
            })
            .expect("read");
        (out, seen)
    }

    fn varint(bytes: &[u8], at: &mut usize) -> u64 {
        let mut value = 0;
        for shift in (0..64).step_by(7) {
            let byte = bytes[*at];
            *at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                break;
            }
        }
        value
    }

    /// The data field a `Blob` message carries: 1 raw, 3 zlib, 7 zstd
    /// (`raw_size`, field 2, is skipped).
    fn blob_data_field(blob: &[u8]) -> u64 {
        let mut i = 0;
        while i < blob.len() {
            let key = varint(blob, &mut i);
            match (key >> 3, key & 7) {
                (number @ (1 | 3 | 7), 2) => return number,
                (_, 2) => i += usize::try_from(varint(blob, &mut i)).expect("len"),
                (_, 0) => {
                    varint(blob, &mut i);
                }
                other => panic!("unexpected Blob field {other:?}"),
            }
        }
        panic!("a Blob without data")
    }

    /// The data field of each data blob's `Blob` message. Reads the
    /// framing directly, since what the writer chose is what is under test.
    fn data_blob_codecs(path: &Path) -> Vec<u64> {
        let bytes = std::fs::read(path).expect("read");
        let mut at = 0;
        let mut codecs = Vec::new();
        while at < bytes.len() {
            let len = u32::from_be_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
            at += 4;
            let header = &bytes[at..at + len as usize];
            at += len as usize;
            let (mut i, mut kind, mut datasize) = (0, Vec::new(), 0);
            while i < header.len() {
                let key = varint(header, &mut i);
                match (key >> 3, key & 7) {
                    (1, 2) => {
                        let n = usize::try_from(varint(header, &mut i)).expect("len");
                        kind = header[i..i + n].to_vec();
                        i += n;
                    }
                    (3, 0) => datasize = usize::try_from(varint(header, &mut i)).expect("size"),
                    (_, 2) => i += usize::try_from(varint(header, &mut i)).expect("len"),
                    (_, 0) => {
                        varint(header, &mut i);
                    }
                    other => panic!("unexpected BlobHeader field {other:?}"),
                }
            }
            if kind == b"OSMData" {
                codecs.push(blob_data_field(&bytes[at..at + datasize]));
            }
            at += datasize;
        }
        codecs
    }

    const ARMS: [(Writer, Compression, u64); 6] = [
        (Writer::Sync, Compression::None, 1),
        (Writer::Sync, Compression::Zlib(6), 3),
        (Writer::Sync, Compression::Zstd(3), 7),
        (Writer::Pipelined, Compression::None, 1),
        (Writer::Pipelined, Compression::Zlib(6), 3),
        (Writer::Pipelined, Compression::Zstd(3), 7),
    ];

    const COUNTS: Counts = Counts {
        nodes: 8_100,
        ways: 3,
        relations: 2,
    };

    /// Every arm writes a complete, flushed PBF in its codec whose
    /// elements are the input's, metadata dropped.
    #[test]
    fn every_encode_arm_round_trips_what_it_keeps() {
        let scratch = pbfhogg_test_scratch::scratch();
        let input = scratch.child("input.osm.pbf");
        write_input(&input);
        let (expected, input_droppable) = kept(&input);
        assert_eq!(
            input_droppable,
            Droppable {
                node_metadata: true,
                way_metadata: true,
                relation_metadata: true,
                way_locations: true,
            },
            "the fixture carries everything encode drops"
        );
        for (writer, compression, codec) in ARMS {
            let sink = scratch.child(format!("{writer:?}-{compression:?}.osm.pbf"));
            let counts = encode(&input, &sink, writer, compression, &plain_host()).expect("encode");
            assert_eq!(counts, COUNTS, "{writer:?} {compression:?}");
            let (got, droppable) = kept(&sink);
            assert_eq!(got, expected, "{writer:?} {compression:?}");
            assert_eq!(
                droppable,
                Droppable::default(),
                "{writer:?} {compression:?} kept what it drops"
            );
            let codecs = data_blob_codecs(&sink);
            assert!(codecs.len() >= 4, "{codecs:?}");
            assert!(
                codecs.iter().all(|&c| c == codec),
                "{writer:?} {compression:?}: {codecs:?}"
            );
        }
    }

    /// Every reader counts the same elements.
    #[test]
    fn every_reader_counts_the_input() {
        let scratch = pbfhogg_test_scratch::scratch();
        let input = scratch.child("input.osm.pbf");
        write_input(&input);
        for reader in [
            Reader::Sequential,
            Reader::Parallel,
            Reader::Pipelined,
            Reader::Blobreader,
        ] {
            assert_eq!(
                decode(&input, reader, &plain_host()).expect("decode"),
                COUNTS,
                "{reader:?}"
            );
        }
    }

    /// A header-less input fails every arm instead of counting nothing.
    #[test]
    fn a_header_less_input_fails_every_arm() {
        let scratch = pbfhogg_test_scratch::scratch();
        let input = scratch.child("empty.osm.pbf");
        std::fs::write(&input, b"").expect("empty");
        for reader in [
            Reader::Sequential,
            Reader::Parallel,
            Reader::Pipelined,
            Reader::Blobreader,
        ] {
            assert!(decode(&input, reader, &plain_host()).is_err(), "{reader:?}");
        }
        for (writer, compression, _) in ARMS {
            let sink = scratch.child("sink.osm.pbf");
            let result = encode(&input, &sink, writer, compression, &plain_host());
            assert!(result.is_err(), "{writer:?} {compression:?}");
        }
    }

    fn varint_into(mut value: u64, out: &mut Vec<u8>) {
        while value >= 0x80 {
            out.push(u8::try_from(value & 0x7f).expect("seven bits") | 0x80);
            value >>= 7;
        }
        out.push(u8::try_from(value).expect("seven bits"));
    }

    fn field(number: u64, wire: u64, out: &mut Vec<u8>) {
        varint_into(number * 8 + wire, out);
    }

    fn len_prefixed(bytes: &[u8], out: &mut Vec<u8>) {
        varint_into(u64::try_from(bytes.len()).expect("len"), out);
        out.extend_from_slice(bytes);
    }

    fn bytes_field(number: u64, bytes: &[u8], out: &mut Vec<u8>) {
        field(number, 2, out);
        len_prefixed(bytes, out);
    }

    /// A relation 1 whose one member's role string index (9) is past its
    /// block's string table (only "").
    fn write_bad_role(path: &Path) {
        let mut writer = PbfWriter::new(
            std::fs::File::create(path).expect("create"),
            Compression::default(),
        );
        writer
            .write_header(&HeaderBuilder::new().build().expect("header"))
            .expect("header");
        let mut relation = Vec::new();
        field(1, 0, &mut relation);
        relation.push(1); // id 1
        bytes_field(8, &[9], &mut relation); // roles_sid
        bytes_field(9, &[2], &mut relation); // memids: node 1, zigzag delta
        bytes_field(10, &[0], &mut relation); // types: node
        let mut group = Vec::new();
        bytes_field(4, &relation, &mut group);
        let mut block = Vec::new();
        bytes_field(1, &[10, 0], &mut block); // string table: just ""
        bytes_field(2, &group, &mut block);
        writer.write_primitive_block(&block).expect("block");
        writer.flush().expect("flush");
    }

    /// A bad role refuses the call under both writers instead of encoding
    /// an empty role: a revision whose role decoding fails does less work
    /// and must never measure as faster.
    #[test]
    fn encode_refuses_a_bad_role_under_both_writers() {
        let scratch = pbfhogg_test_scratch::scratch();
        let input = scratch.child("bad-role.osm.pbf");
        write_bad_role(&input);
        for writer in [Writer::Sync, Writer::Pipelined] {
            let sink = scratch.child("sink.osm.pbf");
            let error = encode(&input, &sink, writer, Compression::None, &plain_host())
                .expect_err("a bad role refuses");
            let report = error.report();
            assert!(
                report.contains("relation 1 member 0: role string index 9"),
                "{writer:?}: {report}"
            );
        }
    }

    /// The encode reader reports through the host's diagnostics, as the
    /// commands' readers do: a malformed indexdata warns once.
    #[test]
    fn encode_reports_unrecognised_indexdata() {
        let scratch = pbfhogg_test_scratch::scratch();
        let input = scratch.child("bad-indexdata.osm.pbf");
        let mut writer = PbfWriter::new(
            std::fs::File::create(&input).expect("create"),
            Compression::default(),
        );
        writer
            .write_header(&HeaderBuilder::new().build().expect("header"))
            .expect("header");
        writer.flush().expect("flush");
        drop(writer);
        let mut bb = BlockBuilder::new();
        bb.add_node(1, 0, 0, std::iter::empty(), None);
        let block = bb.take().expect("take").expect("a block").to_vec();
        // One raw data blob framed by hand, its header carrying three bytes
        // of indexdata: no recognised layout has that length.
        let mut blob = Vec::new();
        field(1, 2, &mut blob);
        len_prefixed(&block, &mut blob);
        field(2, 0, &mut blob);
        varint_into(u64::try_from(block.len()).expect("len"), &mut blob);
        let mut header = Vec::new();
        bytes_field(1, b"OSMData", &mut header);
        bytes_field(2, &[1, 2, 3], &mut header);
        field(3, 0, &mut header);
        varint_into(u64::try_from(blob.len()).expect("len"), &mut header);
        let mut frame = u32::try_from(header.len())
            .expect("len")
            .to_be_bytes()
            .to_vec();
        frame.extend_from_slice(&header);
        frame.extend_from_slice(&blob);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&input)
            .expect("append");
        std::io::Write::write_all(&mut file, &frame).expect("frame");
        drop(file);
        for writer in [Writer::Sync, Writer::Pipelined] {
            let sink = Arc::new(Recording::default());
            let host = host(Diagnostics::new(
                Arc::clone(&sink) as Arc<dyn pbfhogg::host::DiagnosticSink>
            ));
            let sink_path = scratch.child("sink.osm.pbf");
            encode(&input, &sink_path, writer, Compression::None, &host).expect("encode");
            let warnings = sink
                .0
                .lock()
                .expect("sink")
                .iter()
                .filter(|m| m.contains("indexdata has an unrecognised length"))
                .count();
            assert_eq!(warnings, 1, "{writer:?}");
        }
    }
}
