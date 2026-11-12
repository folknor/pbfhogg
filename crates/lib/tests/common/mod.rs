// Mounted once by `crates/lib/tests/all.rs`, once by each separate fault-test binary,
// and once by the CLI package's `crates/cli/tests/all.rs` (by `#[path]`, so its
// `cfg(feature = ...)` gates read the mounting package's features, which
// carry the same names). Every binary uses only a subset of helpers, and
// feature-gated modules drop out of some sweeps, so unused warnings are
// expected and harmless.
//
// CAUTION: the features do NOT all mean the same in both packages.
// `commands` is on by default in `pbfhogg` but is a no-op feature in
// `pbfhogg-cli`, off in every sweep but `consumer`; `geocode-reader` is
// likewise absent there. A gate on either in anything under crates/lib/tests/common/
// would silently drop CLI tests out of those sweeps. Gate the `mod` line in
// crates/lib/tests/all.rs instead; the `no-library-feature-gates-in-tests-common`
// textlint rule in brokkr.toml enforces this.
#![allow(dead_code)]
//! Shared test helpers used across integration tests.
//!
//! This module centralizes the duplicated helper structs and functions that were
//! previously copy-pasted across merge.rs, derive_changes.rs, extract.rs,
//! getid.rs, tags_filter.rs, diff.rs, and add_locations_to_ways.rs.
//!
//! There are two variants of PBF-reading helpers because some tests need node
//! coordinates (lat/lon) while others only need IDs and tags:
//!
//! - [`PbfContentsWithCoords`] / [`read_all_elements_with_coords`]: used by
//!   merge and derive_changes tests that verify coordinate values.
//! - [`PbfContentsIdOnly`] / [`read_all_elements_id_only`]: used by extract,
//!   getid, and tags_filter tests that only need element IDs and tags.
//!
//! Nothing here spawns pbfhogg; the oracle runner spawns external reference tools. Tests that drive
//! it live in the CLI package, whose `crates/cli/tests/invoker.rs` holds the
//! `CliInvoker`.

pub mod adversarial;
pub mod groups;
pub mod model;
pub mod oracle;
pub mod process;

use std::path::Path;

#[allow(unused_imports)]
pub use model::{
    MemberKind, NormalizedMember, NormalizedMeta, NormalizedNode, NormalizedRelation, NormalizedWay,
};

use pbfhogg::block_builder::{self, BlockBuilder, MemberData};
use pbfhogg::writer::{Compression, PbfWriter};
use pbfhogg::{BlobDecode, BlobReader, Element, ElementReader, MemberId};

// Scratch comes from the dev-only `pbfhogg-test-scratch` crate, the one
// owner for every harness; this name is only the integration tests' handle.
#[allow(unused_imports)]
pub use pbfhogg_test_scratch::{TestScratch, scratch as test_scratch};

/// A fresh output at `path` for a library command: refused if anything is
/// already there, so a test that writes one path twice says so with
/// [`out_replace`].
#[track_caller]
pub fn out(path: impl AsRef<Path>) -> pbfhogg::Output {
    pbfhogg::Output::new(
        path.as_ref(),
        pbfhogg::Overwrite::Refuse,
        &pbfhogg::host::Diagnostics::discard(),
    )
    .unwrap_or_else(|e| panic!("stage output {}: {}", path.as_ref().display(), e.report()))
}

/// An output at `path` that replaces whatever file is already there.
#[track_caller]
pub fn out_replace(path: impl AsRef<Path>) -> pbfhogg::Output {
    pbfhogg::Output::new(
        path.as_ref(),
        pbfhogg::Overwrite::Replace,
        &pbfhogg::host::Diagnostics::discard(),
    )
    .unwrap_or_else(|e| panic!("stage output {}: {}", path.as_ref().display(), e.report()))
}

/// A FIFO named `name` in `dir`: the one place integration tests create
/// FIFOs (input-pinning refusals, and `fifo_capture::FifoCapture`).
#[cfg(unix)]
pub fn make_fifo(dir: &Path, name: &str) -> std::path::PathBuf {
    let fifo = dir.join(name);
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("fifo path");
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");
    fifo
}

// The promise guard (crates/cli/tests/guard.rs) mounts skip.rs on its own,
// because the guard-portability crate has no `common`.
#[allow(clippy::duplicate_mod)]
mod skip;
#[allow(unused_imports)]
pub use skip::{Backend, backend_unavailable, fail_if_required, skip};

// Only the library `all` binary runs descriptor-heavy scenarios.
#[allow(dead_code)]
pub(crate) mod fd_gate;
#[allow(unused_imports)]
pub(crate) use fd_gate::{FdTicket, heavy_host, heavy_ticket};

// ---------------------------------------------------------------------------
// Test element structs - lightweight descriptions of OSM elements for building
// test PBF files. These mirror the PBF data model but use static strings for
// convenience.
// ---------------------------------------------------------------------------

/// Optional test-side element metadata. Every field mirrors
/// [`pbfhogg::block_builder::Metadata`] but uses `&'static str` for `user` so
/// struct literals stay cheap. `Default` gives a valid, stable set of values
/// suitable for tests that just want "some metadata" (version 1, epoch 0,
/// changeset 0, uid 0, user "", visible).
#[derive(Clone, Copy, Debug)]
pub struct TestMeta {
    pub version: i32,
    pub timestamp: i64,
    pub changeset: i64,
    pub uid: i32,
    pub user: &'static str,
    pub visible: bool,
}

impl Default for TestMeta {
    fn default() -> Self {
        Self {
            version: 1,
            timestamp: 0,
            changeset: 0,
            uid: 0,
            user: "",
            visible: true,
        }
    }
}

fn to_block_meta(m: &TestMeta) -> block_builder::Metadata<'_> {
    block_builder::Metadata {
        version: m.version,
        timestamp: m.timestamp,
        changeset: m.changeset,
        uid: m.uid,
        user: m.user,
        visible: m.visible,
    }
}

/// A test node with id, coordinates (in decimicrodegrees), tags, and
/// optional metadata.
pub struct TestNode<'a> {
    pub id: i64,
    /// Latitude in decimicrodegrees (10^-7 degrees).
    pub lat: i32,
    /// Longitude in decimicrodegrees (10^-7 degrees).
    pub lon: i32,
    pub tags: Vec<(&'a str, &'a str)>,
    /// Optional per-element metadata (version/timestamp/changeset/uid/user/visible).
    /// `None` writes a node with no OSM metadata - the default for most tests.
    pub meta: Option<TestMeta>,
}

/// A test way with id, node references, tags, and optional metadata.
pub struct TestWay<'a> {
    pub id: i64,
    pub refs: Vec<i64>,
    pub tags: Vec<(&'a str, &'a str)>,
    pub meta: Option<TestMeta>,
}

/// A test relation with id, members, tags, and optional metadata.
///
/// Members use the [`TestMember`] struct which pairs a [`MemberId`] with a role
/// string.
pub struct TestRelation<'a> {
    pub id: i64,
    pub members: Vec<TestMember>,
    pub tags: Vec<(&'a str, &'a str)>,
    pub meta: Option<TestMeta>,
}

/// A single member of a test relation: a typed member id plus a role string.
pub struct TestMember {
    pub id: MemberId,
    pub role: &'static str,
}

// ---------------------------------------------------------------------------
// PBF writing helper - builds a complete PBF file from test element slices.
// ---------------------------------------------------------------------------

/// Write a complete PBF file containing the given nodes, ways, and relations.
///
/// Creates a header blob followed by the blocks of
/// [`encode_fixture_blocks`], through the sync writer with `indexdata` and
/// `tagdata` on every data blob. When `sorted` is true, the header declares
/// `Sort.Type_then_ID`.
///
/// This is the canonical test PBF writer shared across most integration tests.
pub fn write_test_pbf_impl(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
    sorted: bool,
    block_size_override: Option<usize>,
) {
    let blocks = encode_fixture_blocks(nodes, ways, relations, block_size_override);
    let framing = if sorted {
        FixtureFraming::INDEXED
    } else {
        FixtureFraming::INDEXED.unsorted()
    };
    frame_fixture_blocks(path, &blocks, framing);
}

/// [`write_multi_block_test_pbf`] without `indexdata` or `tagdata` on any
/// blob: the same blocks, so a fixture pair from the two writers differs only
/// in whether the merge paths can use the BlobHeader indexes.
pub fn write_multi_block_non_indexed_pbf(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
    block_size: usize,
) {
    assert!(block_size > 0, "block_size must be > 0");
    let blocks = encode_fixture_blocks(nodes, ways, relations, Some(block_size));
    frame_fixture_blocks(path, &blocks, FixtureFraming::NON_INDEXED);
}

/// The data blocks of a fixture, as serialized `PrimitiveBlock`s: nodes,
/// then ways, then relations, each kind starting a new block, a block closed
/// when it reaches capacity (8000 entities by default) or, with
/// `block_size_override`, every N elements of a kind. The override produces
/// multi-block fixtures that exercise batch boundaries, blob classification
/// and passthrough coalescing without thousands of elements.
///
/// Every fixture writer encodes through this, so writers that differ only in
/// framing write the same blocks by construction.
pub fn encode_fixture_blocks(
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
    block_size_override: Option<usize>,
) -> Vec<Vec<u8>> {
    struct Blocks {
        bb: BlockBuilder,
        out: Vec<Vec<u8>>,
        in_block: usize,
        cap: Option<usize>,
    }
    impl Blocks {
        /// Close the open block if `fits` says the next element does not
        /// fit or the override count is reached.
        fn before_add(&mut self, fits: bool) {
            let hit = matches!(self.cap, Some(cap) if self.in_block >= cap);
            if !fits || hit {
                self.close();
            }
            self.in_block += 1;
        }
        fn close(&mut self) {
            if let Some(bytes) = self.bb.take().expect("take") {
                self.out.push(bytes.to_vec());
            }
            self.in_block = 0;
        }
    }

    let mut blocks = Blocks {
        bb: BlockBuilder::new(),
        out: Vec::new(),
        in_block: 0,
        cap: block_size_override,
    };
    for n in nodes {
        let fits = blocks.bb.can_add_node();
        blocks.before_add(fits);
        let meta = n.meta.as_ref().map(to_block_meta);
        blocks
            .bb
            .add_node(n.id, n.lat, n.lon, n.tags.iter().copied(), meta.as_ref());
    }
    blocks.close();
    for w in ways {
        let fits = blocks.bb.can_add_way();
        blocks.before_add(fits);
        let meta = w.meta.as_ref().map(to_block_meta);
        blocks
            .bb
            .add_way(w.id, w.tags.iter().copied(), &w.refs, meta.as_ref());
    }
    blocks.close();
    for r in relations {
        let fits = blocks.bb.can_add_relation();
        blocks.before_add(fits);
        let members: Vec<MemberData<'_>> = r
            .members
            .iter()
            .map(|m| MemberData {
                id: m.id,
                role: m.role,
            })
            .collect();
        let meta = r.meta.as_ref().map(to_block_meta);
        blocks
            .bb
            .add_relation(r.id, r.tags.iter().copied(), &members, meta.as_ref());
    }
    blocks.close();
    blocks.out
}

/// How [`frame_fixture_blocks`] frames a fixture's blocks: the only ways two
/// fixtures of the same elements may differ.
#[derive(Clone, Copy, Debug)]
pub struct FixtureFraming {
    /// The header declares `Sort.Type_then_ID`.
    pub sorted: bool,
    /// Data BlobHeaders carry `indexdata` and `tagdata`.
    pub indexdata: bool,
    /// Written through the pipelined [`PbfWriter::to_path`] rather than the
    /// sync writer.
    pub pipelined: bool,
}

impl FixtureFraming {
    /// Sorted header, indexed blobs, sync writer.
    pub const INDEXED: Self = Self {
        sorted: true,
        indexdata: true,
        pipelined: false,
    };
    /// Sorted header, blobs without `indexdata` or `tagdata`, sync writer.
    pub const NON_INDEXED: Self = Self {
        sorted: true,
        indexdata: false,
        pipelined: false,
    };
    /// Sorted header, indexed blobs, pipelined writer.
    pub const PIPELINED: Self = Self {
        sorted: true,
        indexdata: true,
        pipelined: true,
    };

    /// The same framing under a header that claims no sort order.
    #[must_use]
    pub const fn unsorted(self) -> Self {
        Self {
            sorted: false,
            ..self
        }
    }
}

/// Write `blocks` (from [`encode_fixture_blocks`]) as a complete PBF under
/// `framing`, with the default compression.
pub fn frame_fixture_blocks(path: &Path, blocks: &[Vec<u8>], framing: FixtureFraming) {
    fn write_all<W: std::io::Write>(mut writer: PbfWriter<W>, blocks: &[Vec<u8>], indexdata: bool) {
        for bytes in blocks {
            if indexdata {
                writer.write_primitive_block(bytes).expect("write block");
            } else {
                writer
                    .write_primitive_block_no_indexdata(bytes)
                    .expect("write block");
            }
        }
        writer.flush().expect("flush");
    }

    let mut hb = block_builder::HeaderBuilder::new();
    if framing.sorted {
        hb = hb.sorted();
    }
    let header = hb.build().expect("build header");
    if framing.pipelined {
        let writer = PbfWriter::to_path(
            path,
            Compression::default(),
            &header,
            crate::common::test_executor(),
        )
        .expect("create writer");
        write_all(writer, blocks, framing.indexdata);
    } else {
        let file = std::fs::File::create(path).expect("create file");
        let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
        let mut writer = PbfWriter::new(buf, Compression::default());
        writer.write_header(&header).expect("write header");
        write_all(writer, blocks, framing.indexdata);
    }
}

/// Write a test PBF without the sorted header flag. See [`write_test_pbf_impl`].
pub fn write_test_pbf(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
) {
    write_test_pbf_impl(path, nodes, ways, relations, false, None);
}

/// Write a test PBF with `Sort.Type_then_ID` header flag. See [`write_test_pbf_impl`].
pub fn write_test_pbf_sorted(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
) {
    write_test_pbf_impl(path, nodes, ways, relations, true, None);
}

/// Write a sorted test PBF that forces a block flush every `block_size`
/// elements within each type section. Produces a multi-block fixture with
/// a small, predictable number of blobs, useful for exercising batch
/// boundaries, blob classification, passthrough coalescing, and
/// merge-join block-pair logic without needing ~8000+ elements per type.
///
/// Example: `write_multi_block_test_pbf(path, &nodes_100, &ways_20, &rels_5, 25)`
/// lays the nodes out as four blocks of 25, ways as one block, and relations
/// as one block - six data blobs total.
pub fn write_multi_block_test_pbf(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
    block_size: usize,
) {
    assert!(block_size > 0, "block_size must be > 0");
    write_test_pbf_impl(path, nodes, ways, relations, true, Some(block_size));
}

/// A general-purpose sorted, indexed input: 300 tagged-in-part nodes,
/// 40 ways and 5 multipolygon relations, 100 elements per block, so every
/// kind spans more than one blob or sits in its own.
pub fn write_base_pbf(path: &Path) {
    let (nodes, ways, relations) = base_elements();
    write_multi_block_test_pbf(path, &nodes, &ways, &relations, 100);
}

/// [`write_base_pbf`]'s elements and blocks without `indexdata` on any blob.
pub fn write_base_pbf_non_indexed(path: &Path) {
    let (nodes, ways, relations) = base_elements();
    write_multi_block_non_indexed_pbf(path, &nodes, &ways, &relations, 100);
}

/// [`write_base_pbf`] with node 7 and way 40 deleted and node 301 added,
/// for diff and dedupe inputs.
pub fn write_changed_pbf(path: &Path) {
    let (mut nodes, mut ways, relations) = base_elements();
    nodes.retain(|n| n.id != 7);
    nodes.extend(generate_nodes(1, 301));
    ways.retain(|w| w.id != 40);
    write_multi_block_test_pbf(path, &nodes, &ways, &relations, 100);
}

/// [`write_base_pbf`] with the first data blob's indexdata replaced by 30
/// zero bytes, which no indexdata version recognises.
pub fn write_bad_indexdata_pbf(path: &Path) {
    write_base_pbf(path);
    let bytes = std::fs::read(path).expect("read base fixture");
    let bad = adversarial::mutate_blob_header_indexdata(&bytes, 1, |v| *v = vec![0; 30]);
    std::fs::write(path, bad).expect("write bad-indexdata fixture");
}

fn base_elements() -> (
    Vec<TestNode<'static>>,
    Vec<TestWay<'static>>,
    Vec<TestRelation<'static>>,
) {
    let mut nodes = generate_nodes(300, 1);
    let mut ways = generate_ways(40, 1, 5, 1);
    let mut relations = generate_relations(5, 1, 2, 1);
    for n in &mut nodes {
        if n.id % 10 == 0 {
            n.tags.push(("amenity", "bench"));
        }
    }
    for w in &mut ways {
        if w.id % 2 == 1 {
            w.tags.push(("highway", "residential"));
        }
    }
    for r in &mut relations {
        r.tags.push(("type", "multipolygon"));
    }
    (nodes, ways, relations)
}

/// Write `node_blocks` blocks of three nodes each, then one block of two
/// ways and one block of one relation, through the sync writer. Small
/// blocks and many of them, for exercising tiny pipelined-reader caps
/// (`read_ahead(1)`, `decode_ahead(1)`) and the decode-admission hook.
pub fn write_many_block_pbf(path: &Path, node_blocks: usize) {
    let file = std::fs::File::create(path).expect("create pbf");
    let mut writer = PbfWriter::new(file, Compression::default());

    let header = block_builder::HeaderBuilder::new()
        .bbox(9.0, 54.0, 13.0, 58.0)
        .build()
        .expect("build header");
    writer.write_header(&header).expect("write header");

    let mut bb = BlockBuilder::new();
    let mut node_id = 100_i64;
    for _ in 0..node_blocks {
        for offset in 0..3 {
            bb.add_node(
                node_id,
                550_000_000 + offset * 10_000,
                120_000_000 + offset * 10_000,
                [("name", "node")],
                None,
            );
            node_id += 100;
        }
        writer
            .write_primitive_block(bb.take().expect("take").expect("node block"))
            .expect("write node block");
    }

    bb.add_way(1000, [("highway", "primary")], &[100, 200, 300], None);
    bb.add_way(2000, [("building", "yes")], &[200, 300, 200], None);
    writer
        .write_primitive_block(bb.take().expect("take").expect("way block"))
        .expect("write way block");

    bb.add_relation(
        5000,
        [("type", "multipolygon")],
        &[MemberData {
            id: MemberId::Way(1000),
            role: "outer",
        }],
        None,
    );
    writer
        .write_primitive_block(bb.take().expect("take").expect("relation block"))
        .expect("write relation block");

    writer.flush().expect("flush");
}

/// Write a sorted PBF through the pipelined writer ([`PbfWriter::to_path`]),
/// every data blob carrying `indexdata` and `tagdata`. Its blocks are
/// [`write_test_pbf_sorted`]'s; only the writer differs. Allowlist-only.
pub fn write_indexed_pbf(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
) {
    let blocks = encode_fixture_blocks(nodes, ways, relations, None);
    frame_fixture_blocks(path, &blocks, FixtureFraming::PIPELINED);
}

/// Write a sorted test PBF whose OsmData blobs carry neither `indexdata` nor
/// `tagdata`, for the read paths that only fire on non-indexed input (most
/// visibly `diff_element_stream` in `commands::diff`). Its blocks are
/// [`write_test_pbf_sorted`]'s; only the BlobHeaders differ. The header
/// declares `Sort.Type_then_ID` so consumers that require sorted inputs
/// still accept the output.
pub fn write_test_pbf_non_indexed(
    path: &Path,
    nodes: &[TestNode],
    ways: &[TestWay],
    relations: &[TestRelation],
) {
    let blocks = encode_fixture_blocks(nodes, ways, relations, None);
    frame_fixture_blocks(path, &blocks, FixtureFraming::NON_INDEXED);
}

// ---------------------------------------------------------------------------
// Fixture generation helpers
// ---------------------------------------------------------------------------
//
// These produce sequential, id-sorted element vectors intended to be paired
// with `write_multi_block_test_pbf` when a test wants to exercise multi-blob
// paths without hand-writing dozens of literals. Coordinates are chosen to
// stay inside the WGS84 range and to produce visibly distinct lat/lon per
// element so failures aren't hidden by coordinate collisions.

/// Generate `count` nodes with ids `start_id..start_id+count`.
///
/// Latitudes and longitudes are a deterministic function of **id**, not
/// of the position in the returned vector: `lat = lon = 1000 * (id %
/// 10_000)` decimicrodegrees. Keying off id means two `generate_nodes`
/// calls that produce the same id (even with different `count` /
/// `start_id` windows) agree on the coord - important for tests that
/// splice overlapping id ranges between old/new fixtures and rely on
/// element-equality semantics (diff, merge). Coordinates wrap every
/// 10 000 ids to stay inside the WGS84 range for arbitrarily large
/// inputs. No tags, no metadata - map over the result when a test
/// needs specific tags.
pub fn generate_nodes(count: usize, start_id: i64) -> Vec<TestNode<'static>> {
    (0..count)
        .map(|i| {
            let id_offset = i64::try_from(i).expect("node count fits in i64");
            let id = start_id + id_offset;
            // decimicrodegrees = 1e-7 degrees, so 1e-4° = 1000 decimicrodegrees.
            let step = 1000_i32;
            let wrap = i32::try_from(id.rem_euclid(10_000)).expect("id mod 10_000 fits in i32");
            let coord = step.saturating_mul(wrap);
            TestNode {
                id,
                lat: coord,
                lon: coord,
                tags: vec![],
                meta: None,
            }
        })
        .collect()
}

/// Generate `count` ways with ids `start_id..start_id+count`, each
/// referencing `refs_per_way` consecutive node ids starting at
/// `node_start_id`. Useful for testing blob classification on ways whose
/// refs point into a specific node-id range.
pub fn generate_ways(
    count: usize,
    start_id: i64,
    refs_per_way: usize,
    node_start_id: i64,
) -> Vec<TestWay<'static>> {
    (0..count)
        .map(|i| {
            let id_offset = i64::try_from(i).expect("way count fits in i64");
            let refs: Vec<i64> = (0..refs_per_way)
                .map(|r| {
                    let r = i64::try_from(r).expect("refs_per_way fits in i64");
                    node_start_id + r
                })
                .collect();
            TestWay {
                id: start_id + id_offset,
                refs,
                tags: vec![],
                meta: None,
            }
        })
        .collect()
}

/// Generate `count` relations with ids `start_id..start_id+count`, each
/// containing `members_per_rel` way members starting at `way_start_id`
/// with the role `"outer"`.
pub fn generate_relations(
    count: usize,
    start_id: i64,
    members_per_rel: usize,
    way_start_id: i64,
) -> Vec<TestRelation<'static>> {
    (0..count)
        .map(|i| {
            let id_offset = i64::try_from(i).expect("relation count fits in i64");
            let members: Vec<TestMember> = (0..members_per_rel)
                .map(|m| {
                    let m = i64::try_from(m).expect("members_per_rel fits in i64");
                    TestMember {
                        id: MemberId::Way(way_start_id + m),
                        role: "outer",
                    }
                })
                .collect();
            TestRelation {
                id: start_id + id_offset,
                members,
                tags: vec![],
                meta: None,
            }
        })
        .collect()
}

/// Generate `n_neg + n_pos` nodes with mixed-sign ids in canonical OSM
/// order: `[-1, -2, ..., -n_neg, 1, 2, ..., n_pos]`. Zero is not a valid
/// OSM id and is omitted. Coordinates and tags follow the
/// [`generate_nodes`] pattern.
///
/// Canonical OSM order is `0, -1, -2, ...` (negatives by ascending
/// absolute value) `then 1, 2, ...` (positives ascending) - see
/// `osm_id_cmp` in `crates/lib/src/osm_id.rs`. Used by the negative-ID sweep
/// (`cli_negative_id_invariants.rs`) to drive every command through
/// a mixed-sign fixture in the order canonical-sorted readers expect.
pub fn generate_nodes_with_negatives(n_neg: usize, n_pos: usize) -> Vec<TestNode<'static>> {
    let mut out = Vec::with_capacity(n_neg + n_pos);
    for i in 1..=n_neg {
        let id = -i64::try_from(i).expect("n_neg fits in i64");
        let step = 1000_i32;
        let wrap = i32::try_from(id.rem_euclid(10_000)).expect("id mod 10_000 fits in i32");
        let coord = step.saturating_mul(wrap);
        out.push(TestNode {
            id,
            lat: coord,
            lon: coord,
            tags: vec![],
            meta: None,
        });
    }
    for i in 1..=n_pos {
        let id = i64::try_from(i).expect("n_pos fits in i64");
        let step = 1000_i32;
        let wrap = i32::try_from(id.rem_euclid(10_000)).expect("id mod 10_000 fits in i32");
        let coord = step.saturating_mul(wrap);
        out.push(TestNode {
            id,
            lat: coord,
            lon: coord,
            tags: vec![],
            meta: None,
        });
    }
    out
}

/// Generate `n_neg + n_pos` ways with mixed-sign ids in canonical OSM
/// order: `[-1, -2, ..., -n_neg, 1, 2, ..., n_pos]`. Each way references
/// `refs_per_way` consecutive positive node ids starting at `1` -
/// callers that want way refs targeting negative nodes can layer them
/// on after.
pub fn generate_ways_with_negatives(
    n_neg: usize,
    n_pos: usize,
    refs_per_way: usize,
) -> Vec<TestWay<'static>> {
    let mut out = Vec::with_capacity(n_neg + n_pos);
    let make_refs = |seed: i64| -> Vec<i64> {
        (0..refs_per_way)
            .map(|r| {
                let r = i64::try_from(r).expect("refs_per_way fits in i64");
                seed.saturating_add(r)
            })
            .collect()
    };
    for i in 1..=n_neg {
        let id = -i64::try_from(i).expect("n_neg fits in i64");
        out.push(TestWay {
            id,
            refs: make_refs(1),
            tags: vec![],
            meta: None,
        });
    }
    for i in 1..=n_pos {
        let id = i64::try_from(i).expect("n_pos fits in i64");
        out.push(TestWay {
            id,
            refs: make_refs(1),
            tags: vec![],
            meta: None,
        });
    }
    out
}

/// Generate `n_neg + n_pos` relations with mixed-sign ids in canonical
/// OSM order: `[-1, -2, ..., -n_neg, 1, 2, ..., n_pos]`. Each relation
/// has `members_per_rel` way members with role `"outer"`, referencing
/// positive way ids starting at `1`.
pub fn generate_relations_with_negatives(
    n_neg: usize,
    n_pos: usize,
    members_per_rel: usize,
) -> Vec<TestRelation<'static>> {
    let mut out = Vec::with_capacity(n_neg + n_pos);
    let make_members = || -> Vec<TestMember> {
        (0..members_per_rel)
            .map(|m| {
                let m = i64::try_from(m).expect("members_per_rel fits in i64");
                TestMember {
                    id: MemberId::Way(1 + m),
                    role: "outer",
                }
            })
            .collect()
    };
    for i in 1..=n_neg {
        let id = -i64::try_from(i).expect("n_neg fits in i64");
        out.push(TestRelation {
            id,
            members: make_members(),
            tags: vec![],
            meta: None,
        });
    }
    for i in 1..=n_pos {
        let id = i64::try_from(i).expect("n_pos fits in i64");
        out.push(TestRelation {
            id,
            members: make_members(),
            tags: vec![],
            meta: None,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Indexdata assertions
// ---------------------------------------------------------------------------

/// Assert the PBF's first OsmData blob carries indexdata. Matches the
/// representativeness assumption of `pbfhogg::has_indexdata`: the first
/// data blob is trusted to speak for the rest of the file. `write_test_pbf`
/// and friends always emit indexed blobs, so this is a fast smoke check
/// that a helper hasn't accidentally dropped indexdata.
pub fn assert_indexed(path: &Path) {
    let indexed = pbfhogg::has_indexdata(path, false, &pbfhogg::host::Diagnostics::discard())
        .expect("probe indexdata");
    assert!(
        indexed,
        "expected {} to carry indexdata on its first data blob",
        path.display()
    );
}

/// Assert the PBF's first OsmData blob has no indexdata. Pair with
/// `write_test_pbf_non_indexed` (see the non-indexed writer helper) to
/// confirm the blob really landed without the indexdata field.
pub fn assert_non_indexed(path: &Path) {
    let indexed = pbfhogg::has_indexdata(path, false, &pbfhogg::host::Diagnostics::discard())
        .expect("probe indexdata");
    assert!(
        !indexed,
        "expected {} to have no indexdata on its first data blob",
        path.display()
    );
}

/// Assert every OsmData blob in the PBF carries indexdata, walking every
/// frame. [`assert_indexed`] reads the first data blob only, so it passes a
/// partially indexed file, which every schedule walk refuses.
pub fn assert_every_data_blob_indexed(path: &Path) {
    let bytes = std::fs::read(path).expect("read pbf");
    let stream = adversarial::locate_blobs_strict(&bytes).expect("strict PBF framing");
    assert_eq!(stream.extent, bytes.len());
    let data: Vec<_> = stream
        .frames
        .iter()
        .enumerate()
        .filter(|(_, (_, kind))| kind.as_slice() == b"OSMData")
        .collect();
    assert!(!data.is_empty(), "expected at least one data blob");
    let unindexed: Vec<usize> = data
        .into_iter()
        .filter(|(_, (blob, _))| {
            adversarial::is_unindexed_data_blob(&bytes[blob.header_start..blob.header_end])
        })
        .map(|(i, _)| i)
        .collect();
    assert!(
        unindexed.is_empty(),
        "expected every data blob of {} to carry indexdata; blobs {unindexed:?} do not",
        path.display()
    );
}

/// Assert the PBF's first OsmData blob carries tagdata (`BlobHeader`
/// field 4). Tagdata sibling of [`assert_indexed`]; used to prove a fixture
/// really has a per-blob tag key index before a `--strip-tagdata` run so the
/// removal is a meaningful assertion.
pub fn assert_has_tagdata(path: &Path) {
    let tagged = pbfhogg::has_tagdata(path, false, &pbfhogg::host::Diagnostics::discard())
        .expect("probe tagdata");
    assert!(
        tagged,
        "expected {} to carry tagdata on its first data blob",
        path.display()
    );
}

/// Assert the PBF's first OsmData blob has no tagdata. Pair with
/// `--strip-tagdata` to confirm the field really landed cleared.
pub fn assert_no_tagdata(path: &Path) {
    let tagged = pbfhogg::has_tagdata(path, false, &pbfhogg::host::Diagnostics::discard())
        .expect("probe tagdata");
    assert!(
        !tagged,
        "expected {} to have no tagdata on its first data blob",
        path.display()
    );
}

/// Count OsmData blobs whose `BlobHeader` carries a `tagdata` field (field 4)
/// by walking every blob frame in the file, not just the first.
///
/// `pbfhogg::has_tagdata` trusts the first data blob; this inspects the whole
/// file so a `--strip-tagdata` run can be proven to have cleared *every* blob
/// (and so a fixture can be proven to carry tagdata across more than one
/// blob). Parses the outer PBF envelope directly: each frame is
/// `[4-byte BE header_len][BlobHeader][Blob]`, and the walk reads the
/// `BlobHeader` protobuf just far enough to find `datasize` (field 3, to
/// advance past the Blob) and detect `tagdata` (field 4).
pub fn count_tagdata_blobs(path: &Path) -> usize {
    let bytes = std::fs::read(path).expect("read pbf");
    let blobs = adversarial::locate_blobs(&bytes);
    assert_eq!(
        blobs.last().map_or(0, |blob| blob.blob_end),
        bytes.len(),
        "incomplete PBF framing in {}",
        path.display()
    );
    blobs
        .iter()
        .filter(|blob| {
            adversarial::header_has_tagdata(&bytes[blob.header_start..blob.header_end])
                .expect("valid BlobHeader")
        })
        .count()
}

/// Assert that *no* blob in the PBF carries tagdata, walking every blob
/// frame rather than trusting the first. Strengthens [`assert_no_tagdata`]
/// for `--strip-tagdata` output: a stripper that only cleared the first
/// blob's tagdata would pass the O(1) first-blob probe but fail here.
pub fn assert_no_tagdata_all_blobs(path: &Path) {
    let n = count_tagdata_blobs(path);
    assert_eq!(
        n,
        0,
        "expected no blob in {} to carry tagdata, but {n} still do",
        path.display()
    );
}

// ---------------------------------------------------------------------------
// PBF header reading
// ---------------------------------------------------------------------------

/// Read the header from a PBF file.
pub fn read_header(path: &Path) -> pbfhogg::HeaderBlock {
    let reader = ElementReader::from_path(path).expect("open pbf");
    reader.header().clone()
}

// ---------------------------------------------------------------------------
// PBF reading helpers - "with coords" variant
// ---------------------------------------------------------------------------

/// Collected PBF elements including node coordinates. Used by merge and
/// derive_changes tests that need to verify lat/lon values.
///
/// Tuple layouts:
/// - nodes: `(id, lat, lon, tags)`
/// - ways: `(id, refs, tags)`
/// - relations: `(id, members_as_(id, type_str, role), tags)`
#[derive(Debug)]
#[allow(clippy::type_complexity)]
pub struct PbfContentsWithCoords {
    pub nodes: Vec<(i64, i32, i32, Vec<(String, String)>)>,
    pub ways: Vec<(i64, Vec<i64>, Vec<(String, String)>)>,
    pub relations: Vec<(i64, Vec<(i64, String, String)>, Vec<(String, String)>)>,
}

/// Read all elements from a PBF file, preserving node coordinates.
///
/// Reads through pbfhogg's reader, so its callers exercise the product
/// decoder. Tags are checked (an unresolvable index fails the read instead of
/// ending the list), a role that does not resolve fails it, and an unknown
/// member kind reads `unknown:N` with its raw value. Returns a
/// [`PbfContentsWithCoords`] with all elements in file order.
pub fn read_all_elements_with_coords(path: &Path) -> PbfContentsWithCoords {
    let reader = BlobReader::from_path(path).expect("open pbf");
    let mut contents = PbfContentsWithCoords {
        nodes: Vec::new(),
        ways: Vec::new(),
        relations: Vec::new(),
    };

    for blob in reader {
        let blob = blob.expect("read blob");
        let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") else {
            continue;
        };
        for element in block.elements() {
            match element {
                Element::DenseNode(dn) => {
                    let tags = checked_tags(dn.tags_checked(), &format!("dense node {}", dn.id()));
                    contents
                        .nodes
                        .push((dn.id(), dn.decimicro_lat(), dn.decimicro_lon(), tags));
                }
                Element::Node(n) => {
                    let tags = checked_tags(n.tags_checked(), &format!("node {}", n.id()));
                    contents
                        .nodes
                        .push((n.id(), n.decimicro_lat(), n.decimicro_lon(), tags));
                }
                Element::Way(w) => {
                    let tags = checked_tags(w.tags_checked(), &format!("way {}", w.id()));
                    contents.ways.push((w.id(), w.refs().collect(), tags));
                }
                Element::Relation(r) => {
                    let what = format!("relation {}", r.id());
                    let tags = checked_tags(r.tags_checked(), &what);
                    let members = r
                        .members()
                        .map(|m| {
                            let kind = match member_kind(m.id.member_type()) {
                                MemberKind::Node => "node".to_owned(),
                                MemberKind::Way => "way".to_owned(),
                                MemberKind::Relation => "relation".to_owned(),
                                MemberKind::Unknown(raw) => format!("unknown:{raw}"),
                            };
                            let role = m
                                .role()
                                .unwrap_or_else(|e| panic!("{what}: role: {e}"))
                                .to_owned();
                            (m.id.id(), kind, role)
                        })
                        .collect();
                    contents.relations.push((r.id(), members, tags));
                }
                other => panic!("element kind the oracle does not know: {other:?}"),
            }
        }
    }

    contents
}

/// Extract just the node IDs from a [`PbfContentsWithCoords`].
pub fn node_ids_with_coords(c: &PbfContentsWithCoords) -> Vec<i64> {
    c.nodes.iter().map(|(id, _, _, _)| *id).collect()
}

/// Extract just the way IDs from a [`PbfContentsWithCoords`].
pub fn way_ids_with_coords(c: &PbfContentsWithCoords) -> Vec<i64> {
    c.ways.iter().map(|(id, _, _)| *id).collect()
}

/// Extract just the relation IDs from a [`PbfContentsWithCoords`].
pub fn relation_ids_with_coords(c: &PbfContentsWithCoords) -> Vec<i64> {
    c.relations.iter().map(|(id, _, _)| *id).collect()
}

// ---------------------------------------------------------------------------
// PBF reading helpers - "id only" variant (no coordinates on nodes)
// ---------------------------------------------------------------------------

/// Collected PBF elements without node coordinates. Used by extract, getid,
/// and tags_filter tests that only need IDs and tags.
///
/// Tuple layouts:
/// - nodes: `(id, tags)`
/// - ways: `(id, refs, tags)`
/// - relations: `(id, tags)`
#[derive(Debug)]
#[allow(clippy::type_complexity)]
pub struct PbfContentsIdOnly {
    pub nodes: Vec<(i64, Vec<(String, String)>)>,
    pub ways: Vec<(i64, Vec<i64>, Vec<(String, String)>)>,
    pub relations: Vec<(i64, Vec<(String, String)>)>,
}

/// Read all elements from a PBF file, discarding node coordinates.
///
/// Reads through pbfhogg's reader, as [`read_all_elements_with_coords`]
/// does, with the same refusals. Returns a [`PbfContentsIdOnly`] with all
/// elements in file order.
pub fn read_all_elements_id_only(path: &Path) -> PbfContentsIdOnly {
    let full = read_all_elements_with_coords(path);
    PbfContentsIdOnly {
        nodes: full
            .nodes
            .into_iter()
            .map(|(id, _, _, tags)| (id, tags))
            .collect(),
        ways: full.ways,
        relations: full
            .relations
            .into_iter()
            .map(|(id, _, tags)| (id, tags))
            .collect(),
    }
}

/// Extract just the node IDs from a [`PbfContentsIdOnly`].
pub fn node_ids_id_only(c: &PbfContentsIdOnly) -> Vec<i64> {
    c.nodes.iter().map(|(id, _)| *id).collect()
}

/// Extract just the way IDs from a [`PbfContentsIdOnly`].
pub fn way_ids_id_only(c: &PbfContentsIdOnly) -> Vec<i64> {
    c.ways.iter().map(|(id, _, _)| *id).collect()
}

/// Extract just the relation IDs from a [`PbfContentsIdOnly`].
pub fn relation_ids_id_only(c: &PbfContentsIdOnly) -> Vec<i64> {
    c.relations.iter().map(|(id, _)| *id).collect()
}

// ---------------------------------------------------------------------------
// PBF reading helpers - "normalized" variant for element-equivalence cross-checks
// ---------------------------------------------------------------------------
//
// Byte-identical comparison is too strict for outputs: string-table order,
// dense delta packing and block boundaries vary without any element changing.
// The normalized form is the independent decode in `model`: ids,
// nanodegree coordinates, embedded way locations, tags in stored order with
// duplicates, members with raw unknown kinds, and metadata claims field by
// field with millisecond timestamps. Each section is sorted by id, so outputs
// with the same elements in a different file order compare equal; stream
// order is `assert_sorted_file`'s subject.

/// Reject incomplete framing, including the short trailing prefixes that the
/// production reader deliberately tolerates as EOF. Successful-output oracles
/// need the last complete frame to cover the entire file.
fn assert_complete_pbf_framing(path: &Path) {
    let mut reader = BlobReader::seekable_from_path(path).expect("open framed PBF");
    let mut end = 0;
    while let Some(header) = reader.next_header_skip_blob() {
        header.expect("read complete PBF frame");
        end = reader
            .seek_raw(std::io::SeekFrom::Current(0))
            .expect("frame end");
    }
    assert_eq!(
        end,
        std::fs::metadata(path).expect("PBF length").len(),
        "incomplete trailing PBF frame: {}",
        path.display()
    );
}

/// Normalized view of a complete PBF file: the independent decode
/// ([`model`]), each section sorted by id (stably, so duplicate ids keep
/// file order).
#[derive(Debug)]
pub struct NormalizedPbf {
    pub nodes: Vec<NormalizedNode>,
    pub ways: Vec<NormalizedWay>,
    pub relations: Vec<NormalizedRelation>,
}

/// Read a successful output into its element-equivalence form.
///
/// The file must be completely framed and carry a header. It is decoded
/// twice: by the independent decoder in [`model`], which is what the result
/// holds, and by pbfhogg's reader, whose public view (ids, nanodegree
/// coordinates, checked tags, refs, way locations, members, roles and the
/// metadata its accessors show) must agree with the independent one. So a
/// defect in either decoder fails the read rather than letting two outputs
/// compare equal; a production refusal (a file the product cannot read)
/// fails it too. Counterexamples for the decoder itself use
/// [`model::decode_path`] directly.
pub fn read_normalized(path: &Path) -> NormalizedPbf {
    assert_complete_pbf_framing(path);
    let decoded = model::decode_path(path);
    assert!(decoded.header, "PBF header missing: {}", path.display());

    let mut independent = public_view_of_model(&decoded);
    let mut production = public_view_of_reader(path);
    independent.sort();
    production.sort();
    assert_eq!(
        independent,
        production,
        "pbfhogg's reader and the independent decoder disagree on {}",
        path.display()
    );

    let mut contents = NormalizedPbf {
        nodes: decoded.nodes,
        ways: decoded.ways,
        relations: decoded.relations,
    };
    contents.nodes.sort_by_key(|n| n.id);
    contents.ways.sort_by_key(|w| w.id);
    contents.relations.sort_by_key(|r| r.id);
    contents
}

/// What both decoders can state about one element. Nodes carry only the
/// claimed version and timestamp: a dense slot's other public fields are raw
/// column values, whose claims only the independent decoder derives.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PublicView {
    Node {
        id: i64,
        nano: (i64, i64),
        tags: Vec<(String, String)>,
        version: Option<i32>,
        milli_timestamp: Option<i64>,
    },
    Way {
        id: i64,
        refs: Vec<i64>,
        locations: Option<Vec<(i64, i64)>>,
        tags: Vec<(String, String)>,
        meta: PlainView,
    },
    Relation {
        id: i64,
        members: Vec<(MemberKind, i64, String)>,
        tags: Vec<(String, String)>,
        meta: PlainView,
    },
}

/// A plain `Info` as the public accessors show it; `visible` is left out
/// because they cannot show it absent.
type PlainView = (
    Option<i32>,
    Option<i64>,
    Option<i64>,
    Option<i32>,
    Option<String>,
);

fn plain_view_of_model(meta: Option<&NormalizedMeta>) -> PlainView {
    meta.map_or((None, None, None, None, None), |m| {
        (
            m.version,
            m.milli_timestamp,
            m.changeset,
            m.uid,
            m.user.clone(),
        )
    })
}

fn plain_view_of_info(info: &pbfhogg::Info<'_>, what: &str) -> PlainView {
    (
        info.claimed_version(),
        info.milli_timestamp(),
        info.changeset(),
        info.uid(),
        info.user().map(|user| {
            user.unwrap_or_else(|e| panic!("{what}: user: {e}"))
                .to_owned()
        }),
    )
}

fn public_view_of_model(decoded: &model::DecodedPbf) -> Vec<PublicView> {
    let nodes = decoded.nodes.iter().map(|n| PublicView::Node {
        id: n.id,
        nano: (n.nano_lat, n.nano_lon),
        tags: n.tags.clone(),
        version: n.meta.as_ref().and_then(|m| m.version),
        milli_timestamp: n.meta.as_ref().and_then(|m| m.milli_timestamp),
    });
    let ways = decoded.ways.iter().map(|w| PublicView::Way {
        id: w.id,
        refs: w.refs.clone(),
        locations: w.locations.clone(),
        tags: w.tags.clone(),
        meta: plain_view_of_model(w.meta.as_ref()),
    });
    let relations = decoded.relations.iter().map(|r| PublicView::Relation {
        id: r.id,
        members: r
            .members
            .iter()
            .map(|m| (m.member_type, m.ref_id, m.role.clone()))
            .collect(),
        tags: r.tags.clone(),
        meta: plain_view_of_model(r.meta.as_ref()),
    });
    nodes.chain(ways).chain(relations).collect()
}

/// Collect a checked tag iterator, failing on an unresolvable index.
fn checked_tags<'a, E: std::fmt::Display>(
    tags: impl Iterator<Item = Result<(&'a str, &'a str), E>>,
    what: &str,
) -> Vec<(String, String)> {
    tags.map(|tag| {
        let (k, v) = tag.unwrap_or_else(|e| panic!("{what}: tag: {e}"));
        (k.to_owned(), v.to_owned())
    })
    .collect()
}

/// A member kind as the public API reports it, unknown values kept.
fn member_kind(kind: pbfhogg::MemberType) -> MemberKind {
    match kind {
        pbfhogg::MemberType::Node => MemberKind::Node,
        pbfhogg::MemberType::Way => MemberKind::Way,
        pbfhogg::MemberType::Relation => MemberKind::Relation,
        pbfhogg::MemberType::Unknown(raw) => MemberKind::Unknown(raw),
        other => panic!("member type the oracle does not know: {other:?}"),
    }
}

fn public_view_of_reader(path: &Path) -> Vec<PublicView> {
    let mut out = Vec::new();
    for blob in BlobReader::from_path(path).expect("open pbf") {
        let blob = blob.expect("read blob");
        let BlobDecode::OsmData(block) = blob.decode().expect("decode blob") else {
            continue;
        };
        for element in block.elements() {
            out.push(match element {
                Element::DenseNode(n) => {
                    let what = format!("dense node {}", n.id());
                    let (version, milli_timestamp) = n.info().map_or((None, None), |i| {
                        let version = i.claimed_version();
                        let ms = i.milli_timestamp();
                        let claimed = if version.is_some() {
                            i.columns().timestamp()
                        } else {
                            ms != 0
                        };
                        (version, claimed.then_some(ms))
                    });
                    PublicView::Node {
                        id: n.id(),
                        nano: (n.nano_lat(), n.nano_lon()),
                        tags: checked_tags(n.tags_checked(), &what),
                        version,
                        milli_timestamp,
                    }
                }
                Element::Node(n) => {
                    let what = format!("node {}", n.id());
                    let info = n.info();
                    PublicView::Node {
                        id: n.id(),
                        nano: (n.nano_lat(), n.nano_lon()),
                        tags: checked_tags(n.tags_checked(), &what),
                        version: info.claimed_version(),
                        milli_timestamp: info.milli_timestamp(),
                    }
                }
                Element::Way(w) => {
                    let what = format!("way {}", w.id());
                    let locations: Vec<(i64, i64)> = w
                        .node_locations()
                        .map(|l| (l.nano_lat(), l.nano_lon()))
                        .collect();
                    PublicView::Way {
                        id: w.id(),
                        refs: w.refs().collect(),
                        locations: (!locations.is_empty()).then_some(locations),
                        tags: checked_tags(w.tags_checked(), &what),
                        meta: plain_view_of_info(&w.info(), &what),
                    }
                }
                Element::Relation(r) => {
                    let what = format!("relation {}", r.id());
                    PublicView::Relation {
                        id: r.id(),
                        members: r
                            .members()
                            .map(|m| {
                                let role = m
                                    .role()
                                    .unwrap_or_else(|e| panic!("{what}: role: {e}"))
                                    .to_owned();
                                (member_kind(m.id.member_type()), m.id.id(), role)
                            })
                            .collect(),
                        tags: checked_tags(r.tags_checked(), &what),
                        meta: plain_view_of_info(&r.info(), &what),
                    }
                }
                other => panic!("element kind the oracle does not know: {other:?}"),
            });
        }
    }
    out
}

/// Assert that a PBF file is sorted by `Sort.Type_then_ID`:
///
/// - The header block declares `is_sorted() == true`.
/// - Elements appear in file order as nodes → ways → relations (no
///   out-of-order type transitions).
/// - Within each type, ids follow canonical OSM order: zero, negative ids
///   by increasing magnitude, then positive ids.
///
/// Check the stream directly: splitting kinds into separate vectors loses
/// interleaving evidence, while normalizing also sorts away ID inversions.
pub fn assert_sorted_file(path: &Path) {
    assert_complete_pbf_framing(path);
    let reader = ElementReader::from_path(path).expect("open sorted PBF");
    assert!(
        reader.header().is_sorted(),
        "PBF header is not declared sorted: {}",
        path.display()
    );
    let mut previous = None;
    reader
        .for_each(|element| {
            let (kind, id) = match element {
                Element::DenseNode(n) => (0u8, n.id()),
                Element::Node(n) => (0u8, n.id()),
                Element::Way(w) => (1u8, w.id()),
                Element::Relation(r) => (2u8, r.id()),
                _ => return,
            };
            // Keep the oracle independent of the production sorting key. Magnitude
            // is unsigned so i64::MIN remains representable.
            let key = (kind, id > 0, id.unsigned_abs());
            if let Some(last) = previous {
                assert!(
                    last <= key,
                    "elements not sorted in {}: {last:?} followed by {key:?}",
                    path.display()
                );
            }
            previous = Some(key);
        })
        .expect("read complete sorted PBF");
}

/// Assert that two PBF files are element-equivalent: [`read_normalized`] of
/// each, compared section by section, metadata claims, embedded way
/// locations and tag order included. A command allowed to rewrite something
/// (DEVIATIONS.md "Re-encoding: absent metadata fields stay absent") is
/// tested against its expected output, not through a weaker comparison.
///
/// Compares sizes before contents so a count mismatch surfaces before the
/// zip stops early.
pub fn assert_elements_equivalent(path_a: &Path, path_b: &Path) {
    let a = read_normalized(path_a);
    let b = read_normalized(path_b);

    assert_eq!(
        a.nodes.len(),
        b.nodes.len(),
        "node count differs: {} vs {}",
        a.nodes.len(),
        b.nodes.len()
    );
    assert_eq!(
        a.ways.len(),
        b.ways.len(),
        "way count differs: {} vs {}",
        a.ways.len(),
        b.ways.len()
    );
    assert_eq!(
        a.relations.len(),
        b.relations.len(),
        "relation count differs: {} vs {}",
        a.relations.len(),
        b.relations.len()
    );

    for (i, (na, nb)) in a.nodes.iter().zip(b.nodes.iter()).enumerate() {
        assert_eq!(na, nb, "node at sorted index {i} differs");
    }
    for (i, (wa, wb)) in a.ways.iter().zip(b.ways.iter()).enumerate() {
        assert_eq!(wa, wb, "way at sorted index {i} differs");
    }
    for (i, (ra, rb)) in a.relations.iter().zip(b.relations.iter()).enumerate() {
        assert_eq!(ra, rb, "relation at sorted index {i} differs");
    }
}
// ---------------------------------------------------------------------------
// Error helpers for platform-specific I/O tests
// ---------------------------------------------------------------------------

/// Whether `err` authorizes dropping an O_DIRECT cell: only the typed
/// direct-io setup refusal does. A runtime EINVAL can indicate an alignment
/// bug, so raw errno alone must never make an I/O correctness test pass
/// without exercising it. A match fails the test where the run requires
/// direct-io (see `skip::fail_if_required`); the caller announces the skip.
#[cfg(feature = "linux-direct-io")]
pub fn direct_io_unavailable(err: &(dyn std::error::Error + 'static)) -> bool {
    let refused = refusal_in(err, &["direct-io"]);
    if refused {
        fail_if_required(Backend::DirectIo, &error_chain(err));
    }
    refused
}

/// Whether `err` authorizes dropping an io_uring cell: the typed io-uring
/// refusal, or a direct-io one, since the ring writer also needs an O_DIRECT
/// output. A match fails the test where the run requires io-uring.
#[cfg(feature = "linux-io-uring")]
pub fn is_uring_unavailable(err: &(dyn std::error::Error + 'static)) -> bool {
    let refused = refusal_in(err, &["io-uring", "direct-io"]);
    if refused {
        fail_if_required(Backend::IoUring, &error_chain(err));
    }
    refused
}

/// Whether `err`, through its source chain and `io::Error` carriers, holds a
/// typed `PlatformUnsupported` refusal labelled with one of `features`.
#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
fn refusal_in(err: &(dyn std::error::Error + 'static), features: &[&str]) -> bool {
    let mut current = Some(err);
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<pbfhogg::Error>()
            && let pbfhogg::ErrorKind::PlatformUnsupported { feature, .. } = error.kind()
            && features.contains(feature)
        {
            return true;
        }
        if let Some(error) = error.downcast_ref::<std::io::Error>() {
            current = error
                .get_ref()
                .map(|inner| inner as &(dyn std::error::Error + 'static));
        } else {
            current = error.source();
        }
    }
    false
}

/// `err` and every source under it, joined with `: `, so a required
/// backend's failure names the step and errno that refused it.
#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut text = err.to_string();
    let mut current = err.source();
    while let Some(error) = current {
        let part = error.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        current = error.source();
    }
    text
}

// ---------------------------------------------------------------------------
// Scratch-dir tracking (fault-injection test support).
//
// Snapshot a directory's recursive contents before and after a command
// runs; the test asserts no leaked temp files on the error path. Pairs
// with the `test-hooks` fault-injection harness in the pipeline modules.
// ---------------------------------------------------------------------------

/// Recursive listing of entries under `root`, relative to `root`.
/// Directories are included so a removed empty subdir shows up in the diff.
/// Symlinks are listed but never followed; inspection and entry errors fail.
pub fn snapshot_dir(root: &Path) -> std::collections::BTreeSet<std::path::PathBuf> {
    let mut out = std::collections::BTreeSet::new();
    snapshot_dir_inner(root, root, &mut out);
    out
}

fn snapshot_dir_inner(
    base: &Path,
    dir: &Path,
    out: &mut std::collections::BTreeSet<std::path::PathBuf>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(error) if dir != base && error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => panic!("read scratch directory {}: {error}", dir.display()),
    };
    for entry in entries {
        let entry = entry.expect("read scratch directory entry");
        let path = entry.path();
        let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
        out.insert(rel);
        if pbfhogg_test_scratch::path_state(&path)
            .expect("inspect scratch entry")
            .is_some_and(|kind| kind.is_dir())
        {
            snapshot_dir_inner(base, &path, out);
        }
    }
}

/// Assert that a post-run scratch snapshot matches the pre-run snapshot.
/// Panics with a diff listing added / removed entries if not equal.
/// Use around commands that manage scratch files internally: any
/// difference indicates a leaked temp file on the test's execution
/// path (typically an error-path leak the cleanup rule missed).
pub fn assert_scratch_unchanged(
    before: &std::collections::BTreeSet<std::path::PathBuf>,
    after: &std::collections::BTreeSet<std::path::PathBuf>,
) {
    let added: Vec<_> = after.difference(before).collect();
    let removed: Vec<_> = before.difference(after).collect();
    if !added.is_empty() || !removed.is_empty() {
        panic!(
            "scratch directory changed across command invocation\n\
             added (leaked): {added:?}\n\
             removed (stolen): {removed:?}"
        );
    }
}

/// Run a closure inside a tracked scratch directory.
///
/// Snapshots `scratch_root` before and after the closure runs, removes
/// every path in `expected_new_paths` from the post-snapshot, and panics
/// if any other paths were added or removed. Returns the closure result.
///
/// Use this to wrap a command invocation that produces known outputs
/// (relative paths under `scratch_root`) but should not leak any
/// intermediate scratch files.
///
/// ```ignore
/// with_tracked_scratch_dir(dir.path(), &[Path::new("output.osm.pbf")], || {
///     CliInvoker::new()
///         .arg("sort")
///         .arg(&input)
///         .arg("-o")
///         .arg(&output)
///         .assert_success();
/// });
/// ```
pub fn with_tracked_scratch_dir<F, R>(scratch_root: &Path, expected_new_paths: &[&Path], f: F) -> R
where
    F: FnOnce() -> R,
{
    let before = snapshot_dir(scratch_root);
    let result = f();
    let mut after = snapshot_dir(scratch_root);
    for ex in expected_new_paths {
        after.remove(*ex);
    }
    assert_scratch_unchanged(&before, &after);
    result
}

/// Explicit host policy for in-process library tests.
/// The same machine on every host, so no test outcome depends on the
/// runner's core count or free memory. A test that needs a different
/// machine sets its own `host::Fixed`.
pub(crate) fn fixed_resources() -> pbfhogg::host::Fixed {
    pbfhogg::host::Fixed {
        parallelism: std::num::NonZeroUsize::new(4),
        memory_available: Some(8 << 30),
    }
}

/// A run scratch directory inside `dir`, for the library entry points that
/// take one by value.
#[track_caller]
pub(crate) fn scratch_in(dir: impl AsRef<Path>) -> pbfhogg::scratch::RunScratch {
    pbfhogg::scratch::RunScratch::create(dir.as_ref(), &test_host())
        .unwrap_or_else(|e| panic!("scratch in {}: {e}", dir.as_ref().display()))
}

/// The descriptor grant every request gets from a heavy test host's
/// provider (`fd_gate::FdTicket`).
pub(crate) const TEST_FD_GRANT: u64 = 1024;

/// A light test host's descriptor provider: each request is granted
/// `min(wanted, TEST_LIGHT_FD_GRANT)` as a host grant, a function of the
/// request alone - never of sibling tests, outstanding reservations, the
/// open count or the process limit. It reserves nothing; the grant is below
/// any descriptor-heavy command's minimum, so such a run needs a host built
/// from an admitted ticket (`fd_gate`). Tests of reserving behaviour build
/// a `host::FdPool`.
pub(crate) struct FixedFdGrant;

impl pbfhogg::host::ProcessActions for FixedFdGrant {
    fn fd_allowance(&self, wanted: u64) -> pbfhogg::process::FdReservation {
        pbfhogg::process::FdReservation::unreserved(pbfhogg::process::FdAllowance::host_grant(
            wanted.min(pbfhogg::TEST_LIGHT_FD_GRANT),
        ))
    }
}

/// An executor pool of the test's own: four workers, matching
/// [`fixed_resources`], so no test's rayon width depends on the runner or
/// competes with a sibling test for the global pool. Dropping the last
/// owner lets the workers exit; it does not join them.
///
/// Left unjoined on purpose, for the reasons on the unit tests'
/// `Executor::for_tests` in `crates/lib/src/host.rs`: a test host is a
/// caller whose pool outlives calls by contract, the worker thread-locals
/// hold only memory and test-only probes, and a joining fixture would add a
/// teardown wait that hangs on any escaped host or writer clone while no
/// test needs it.
pub(crate) fn test_executor() -> pbfhogg::host::Executor {
    pbfhogg::host::Executor::Pool(std::sync::Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("build a test executor pool"),
    ))
}

/// A host for an in-process library test: fixed resources, an executor
/// pool of its own, fixed descriptor grants and a fresh invocation. Builds
/// no process state; the integration binary's descriptor limit is raised
/// once at its start (`crates/lib/tests/all.rs`), never here.
pub(crate) fn test_host() -> pbfhogg::host::Host {
    pbfhogg::host::Host {
        diagnostics: pbfhogg::host::Diagnostics::discard(),
        process: std::sync::Arc::new(FixedFdGrant),
        resources: std::sync::Arc::new(fixed_resources()),
        page_cache: pbfhogg::host::PageCachePolicy::Keep,
        executor: test_executor(),
        // A fresh token per call: a test that trips one cancels only itself,
        // and one that arms its fault plan (`pbfhogg::test_hooks`) faults
        // only its own run.
        invocation: pbfhogg::host::Invocation::new(),
    }
}

/// A small sorted, indexed PBF with one addressed node on one named street:
/// the geocode builder's minimal input, shared by the library's geocode
/// tests and the CLI tests that run `build-geocode-index`.
pub fn write_synthetic_indexed_input(path: &Path) {
    write_test_pbf_sorted(
        path,
        &[
            TestNode {
                id: 1,
                lat: 557_000_000,
                lon: 125_000_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 2,
                lat: 557_000_000,
                lon: 125_010_000,
                tags: vec![],
                meta: None,
            },
            TestNode {
                id: 3,
                lat: 557_000_500,
                lon: 125_005_000,
                tags: vec![
                    ("addr:housenumber", "10"),
                    ("addr:street", "Main Street"),
                    ("addr:postcode", "1234"),
                ],
                meta: None,
            },
        ],
        &[TestWay {
            id: 10,
            refs: vec![1, 2],
            tags: vec![("highway", "residential"), ("name", "Main Street")],
            meta: None,
        }],
        &[],
    );
}
