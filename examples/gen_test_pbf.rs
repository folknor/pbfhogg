//! Generate the `tests/test.osm.pbf` fixture used by doc examples.

use pbfhogg::MemberId;
use pbfhogg::block_builder::{self, BlockBuilder, MemberData, Metadata};
use pbfhogg::writer::{Compression, PbfWriter};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/test.osm.pbf"));
    let mut writer = PbfWriter::to_path_sync(path, Compression::default())?;

    let header = block_builder::HeaderBuilder::new()
        .bbox(9.0, 54.0, 13.0, 58.0)
        .build()?;
    writer.write_header(&header)?;

    // Three nodes, then one way and one relation, each kind in its own block.
    // Doc examples and unit tests in `src/read/` open this fixture and count
    // its elements and blocks, so changing this shape means updating them.
    let mut bb = BlockBuilder::new();
    let meta = Metadata {
        version: 1,
        timestamp: 1_700_000_000,
        changeset: 100,
        uid: 1,
        user: "test",
        visible: true,
    };
    bb.add_node(
        1,
        556_000_000,
        125_600_000,
        [("name", "Test Node")],
        Some(&meta),
    );
    bb.add_node(
        2,
        556_001_000,
        125_601_000,
        std::iter::empty::<(&str, &str)>(),
        Some(&meta),
    );
    bb.add_node(
        3,
        556_002_000,
        125_602_000,
        std::iter::empty::<(&str, &str)>(),
        Some(&meta),
    );
    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(bytes)?;
    }

    // One building=yes way over the three nodes.
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
    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(bytes)?;
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
    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(bytes)?;
    }

    writer.flush()?;
    eprintln!("Generated {}", path.display());
    Ok(())
}
