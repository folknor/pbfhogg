//! Hand-encoded data blocks laid out group by group, for inputs
//! `BlockBuilder` never writes: several groups to a block, kinds in any
//! order across them, several kinds in one group, empty groups and empty
//! dense messages. Elements carry an ID and the least the validator asks of
//! their kind (a node at 0,0, a way referencing node 1, a relation without
//! members), no tags and no metadata, so the string table is the one empty
//! entry.

use std::path::Path;

use super::adversarial::write_varint;

/// One element field of a group, in wire order.
#[derive(Clone, Copy)]
pub enum Field<'a> {
    /// Plain `Node` messages (group field 1), one per ID.
    Nodes(&'a [i64]),
    /// One `DenseNodes` message (group field 2); empty IDs give an empty message.
    Dense(&'a [i64]),
    /// `Way` messages (group field 3), one per ID.
    Ways(&'a [i64]),
    /// `Relation` messages (group field 4), one per ID.
    Relations(&'a [i64]),
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)).cast_unsigned()
}

fn tag(out: &mut Vec<u8>, field: u32, wire_type: u8) {
    write_varint(out, u64::from((field << 3) | u32::from(wire_type)));
}

fn varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    tag(out, field, 0);
    write_varint(out, value);
}

fn bytes_field(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    tag(out, field, 2);
    write_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// A packed `sint64` field of `values`, delta-coded when `delta`.
fn packed_sint(out: &mut Vec<u8>, field: u32, values: &[i64], delta: bool) {
    let mut packed = Vec::new();
    let mut prev = 0i64;
    for &value in values {
        write_varint(
            &mut packed,
            zigzag(if delta { value - prev } else { value }),
        );
        prev = value;
    }
    bytes_field(out, field, &packed);
}

fn encode_field(group: &mut Vec<u8>, field: Field<'_>) {
    match field {
        Field::Nodes(ids) => {
            for &id in ids {
                let mut node = Vec::new();
                varint_field(&mut node, 1, zigzag(id));
                varint_field(&mut node, 8, 0);
                varint_field(&mut node, 9, 0);
                bytes_field(group, 1, &node);
            }
        }
        Field::Dense(ids) => {
            let mut dense = Vec::new();
            if !ids.is_empty() {
                let zeros = vec![0i64; ids.len()];
                packed_sint(&mut dense, 1, ids, true);
                packed_sint(&mut dense, 8, &zeros, true);
                packed_sint(&mut dense, 9, &zeros, true);
            }
            bytes_field(group, 2, &dense);
        }
        Field::Ways(ids) => {
            for &id in ids {
                let mut way = Vec::new();
                varint_field(&mut way, 1, id.cast_unsigned());
                packed_sint(&mut way, 8, &[1], true);
                bytes_field(group, 3, &way);
            }
        }
        Field::Relations(ids) => {
            for &id in ids {
                let mut relation = Vec::new();
                varint_field(&mut relation, 1, id.cast_unsigned());
                bytes_field(group, 4, &relation);
            }
        }
    }
}

/// A serialized `PrimitiveBlock` holding `groups` in order, each group its
/// fields in wire order (an empty slice is an empty group).
pub fn block(groups: &[&[Field<'_>]]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut table = Vec::new();
    bytes_field(&mut table, 1, b"");
    bytes_field(&mut out, 1, &table);
    for fields in groups {
        let mut group = Vec::new();
        for &field in *fields {
            encode_field(&mut group, field);
        }
        bytes_field(&mut out, 2, &group);
    }
    out
}

/// Write `blocks` as unindexed data blobs, in order, under a header that
/// claims no sort order.
pub fn write_unindexed_blocks(path: &Path, blocks: &[Vec<u8>]) {
    use pbfhogg::block_builder::HeaderBuilder;
    use pbfhogg::writer::{Compression, PbfWriter};

    let file = std::fs::File::create(path).expect("create fixture");
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer
        .write_header(&HeaderBuilder::new().build().expect("header"))
        .expect("write header");
    for bytes in blocks {
        writer
            .write_primitive_block_no_indexdata(bytes)
            .expect("write block");
    }
    writer.flush().expect("flush");
}

/// Plain nodes with coordinates and tags, followed by one named street.
/// This bypasses BlockBuilder's dense-only node encoding.
pub fn plain_geocode_block() -> Vec<u8> {
    let strings = [
        "",
        "addr:housenumber",
        "10",
        "addr:street",
        "Main Street",
        "name",
        "highway",
        "residential",
    ];
    let mut table = Vec::new();
    for value in strings {
        bytes_field(&mut table, 1, value.as_bytes());
    }
    let mut group = Vec::new();
    for (id, lat, lon) in [
        (1, 557_000_000, 125_000_000),
        (2, 557_000_000, 125_010_000),
        (3, 557_000_500, 125_005_000),
    ] {
        let mut node = Vec::new();
        varint_field(&mut node, 1, zigzag(id));
        if id == 3 {
            bytes_field(&mut node, 2, &[1, 3]);
            bytes_field(&mut node, 3, &[2, 4]);
        }
        varint_field(&mut node, 8, zigzag(lat));
        varint_field(&mut node, 9, zigzag(lon));
        bytes_field(&mut group, 1, &node);
    }
    let mut way = Vec::new();
    varint_field(&mut way, 1, 10);
    bytes_field(&mut way, 2, &[5, 6]);
    bytes_field(&mut way, 3, &[4, 7]);
    packed_sint(&mut way, 8, &[1, 2], true);
    let mut ways = Vec::new();
    bytes_field(&mut ways, 3, &way);
    let mut out = Vec::new();
    bytes_field(&mut out, 1, &table);
    bytes_field(&mut out, 2, &group);
    bytes_field(&mut out, 2, &ways);
    out
}
