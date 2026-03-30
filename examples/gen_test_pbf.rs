//! Generate the `tests/test.osm.pbf` fixture used by doc examples.

use pbfhogg::block_builder::{self, BlockBuilder, MemberData, Metadata};
use pbfhogg::MemberId;
use pbfhogg::writer::{Compression, PbfWriter};
use std::path::Path;

fn main() {
    let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/test.osm.pbf"));
    let file = std::fs::File::create(path).expect("create file");
    let buf = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut writer = PbfWriter::new(buf, Compression::default());

    let header =
        block_builder::HeaderBuilder::new().bbox(9.0, 54.0, 13.0, 58.0).build()
            .expect("build header");
    writer.write_header(&header).expect("write header");

    // Three nodes (expected by indexed.rs doc tests)
    let mut bb = BlockBuilder::new();
    let meta = Metadata {
        version: 1,
        timestamp: 1_700_000_000,
        changeset: 100,
        uid: 1,
        user: "test",
        visible: true,
    };
    bb.add_node(1, 556_000_000, 125_600_000, [("name", "Test Node")], Some(&meta));
    bb.add_node(2, 556_001_000, 125_601_000, std::iter::empty::<(&str, &str)>(), Some(&meta));
    bb.add_node(3, 556_002_000, 125_602_000, std::iter::empty::<(&str, &str)>(), Some(&meta));
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write nodes");
    }

    // One way with building=yes (expected by indexed.rs read_ways_and_deps doc test)
    let mut bb = BlockBuilder::new();
    bb.add_way(
        1,
        [("building", "yes")],
        &[1, 2, 3],
        Some(&Metadata {
            version: 1,
            timestamp: 1_700_000_000,
            changeset: 100,
            uid: 1,
            user: "test",
            visible: true,
        }),
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer.write_primitive_block(bytes).expect("write ways");
    }

    // One relation
    let mut bb = BlockBuilder::new();
    bb.add_relation(
        1,
        [("type", "multipolygon")],
        &[MemberData {
            id: MemberId::Way(1),
            role: "outer",
        }],
        Some(&Metadata {
            version: 1,
            timestamp: 1_700_000_000,
            changeset: 100,
            uid: 1,
            user: "test",
            visible: true,
        }),
    );
    if let Some(bytes) = bb.take().expect("take") {
        writer
            .write_primitive_block(bytes)
            .expect("write relations");
    }

    writer.flush().expect("flush");
    eprintln!("Generated {}", path.display());
}
