//! Hand-encoded `PrimitiveBlock` bytes for the unit tests of the block
//! projections, for the shapes `BlockBuilder` never writes (plain nodes, a
//! group holding plain and dense nodes, several groups, unknown member
//! types), and the mutation sweep those tests run: every truncation and a
//! handful of byte values at every position, the inputs on which a
//! projection must refuse exactly what block construction refuses.

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

pub(crate) fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub(crate) fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

pub(crate) fn field_varint(out: &mut Vec<u8>, field: u64, v: u64) {
    varint(out, field << 3);
    varint(out, v);
}

pub(crate) fn field_bytes(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    varint(out, (field << 3) | 2);
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

pub(crate) fn packed(values: impl IntoIterator<Item = u64>) -> Vec<u8> {
    let mut out = Vec::new();
    for v in values {
        varint(&mut out, v);
    }
    out
}

/// `values` as a packed `sint64` delta array.
pub(crate) fn deltas(values: &[i64]) -> Vec<u8> {
    let mut prev = 0_i64;
    packed(values.iter().map(|&v| {
        let d = v.wrapping_sub(prev);
        prev = v;
        zigzag(d)
    }))
}

/// A plain node; `tags` are string indices, `info` (version, timestamp,
/// changeset, uid, user_sid).
pub(crate) fn plain_node(
    id: i64,
    lat: i64,
    lon: i64,
    tags: &[(u64, u64)],
    info: Option<[i64; 5]>,
) -> Vec<u8> {
    let mut n = Vec::new();
    field_varint(&mut n, 1, zigzag(id));
    if !tags.is_empty() {
        field_bytes(&mut n, 2, &packed(tags.iter().map(|t| t.0)));
        field_bytes(&mut n, 3, &packed(tags.iter().map(|t| t.1)));
    }
    if let Some([version, timestamp, changeset, uid, user]) = info {
        let mut i = Vec::new();
        field_varint(&mut i, 1, version as u64);
        field_varint(&mut i, 2, timestamp as u64);
        field_varint(&mut i, 3, changeset as u64);
        field_varint(&mut i, 4, uid as u64);
        field_varint(&mut i, 5, user as u64);
        field_bytes(&mut n, 4, &i);
    }
    field_varint(&mut n, 8, zigzag(lat));
    field_varint(&mut n, 9, zigzag(lon));
    n
}

/// A dense message of `(id, lat, lon)` with `keys_vals` as given.
pub(crate) fn dense(nodes: &[(i64, i64, i64)], keys_vals: &[u64]) -> Vec<u8> {
    let ids: Vec<i64> = nodes.iter().map(|n| n.0).collect();
    let lats: Vec<i64> = nodes.iter().map(|n| n.1).collect();
    let lons: Vec<i64> = nodes.iter().map(|n| n.2).collect();
    let mut d = Vec::new();
    field_bytes(&mut d, 1, &deltas(&ids));
    field_bytes(&mut d, 8, &deltas(&lats));
    field_bytes(&mut d, 9, &deltas(&lons));
    if !keys_vals.is_empty() {
        field_bytes(&mut d, 10, &packed(keys_vals.iter().copied()));
    }
    d
}

/// A way with `refs` and one tag of string indices `tag`.
pub(crate) fn way(id: i64, refs: &[i64], tag: (u64, u64)) -> Vec<u8> {
    let mut w = Vec::new();
    field_varint(&mut w, 1, id as u64);
    field_bytes(&mut w, 2, &packed([tag.0]));
    field_bytes(&mut w, 3, &packed([tag.1]));
    field_bytes(&mut w, 8, &deltas(refs));
    w
}

/// A relation whose members are `(role string index, id, raw type)`; a raw
/// type outside 0..=2 is an unknown member type.
pub(crate) fn relation(id: i64, members: &[(u64, i64, u64)]) -> Vec<u8> {
    let mut r = Vec::new();
    field_varint(&mut r, 1, id as u64);
    field_bytes(&mut r, 8, &packed(members.iter().map(|m| m.0)));
    let ids: Vec<i64> = members.iter().map(|m| m.1).collect();
    field_bytes(&mut r, 9, &deltas(&ids));
    field_bytes(&mut r, 10, &packed(members.iter().map(|m| m.2)));
    r
}

/// One `PrimitiveGroup`'s messages.
#[derive(Default)]
pub(crate) struct Group {
    pub(crate) nodes: Vec<Vec<u8>>,
    pub(crate) dense: Option<Vec<u8>>,
    pub(crate) ways: Vec<Vec<u8>>,
    pub(crate) relations: Vec<Vec<u8>>,
}

/// A block of `strings` and `groups`.
pub(crate) fn block(strings: &[&str], groups: &[Group]) -> Vec<u8> {
    let mut st = Vec::new();
    for s in strings {
        field_bytes(&mut st, 1, s.as_bytes());
    }
    let mut out = Vec::new();
    field_bytes(&mut out, 1, &st);
    for group in groups {
        let mut g = Vec::new();
        for n in &group.nodes {
            field_bytes(&mut g, 1, n);
        }
        if let Some(d) = &group.dense {
            field_bytes(&mut g, 2, d);
        }
        for w in &group.ways {
            field_bytes(&mut g, 3, w);
        }
        for r in &group.relations {
            field_bytes(&mut g, 4, r);
        }
        field_bytes(&mut out, 2, &g);
    }
    out
}

/// Valid blocks of every element kind, for the projection parity tests:
/// plain and dense nodes sharing a group, IDs out of order and negative,
/// several groups, ways and relations beside nodes, an unknown member type
/// ahead of a known member, and builder blocks carrying metadata.
#[allow(clippy::too_many_lines, reason = "one literal list of fixtures")]
pub(crate) fn projection_fixtures() -> Vec<(&'static str, Vec<u8>)> {
    use crate::MemberId;
    use crate::block_builder::{BlockBuilder, MemberData, Metadata};

    let strings = ["", "k", "v", "outer", ""];
    let meta = |i: i64| Metadata {
        version: (i % 4) as i32,
        timestamp: i * 37,
        changeset: i % 3,
        uid: (i % 5) as i32,
        user: "",
        visible: true,
    };
    let encode = |build: &dyn Fn(&mut BlockBuilder)| {
        let mut b = BlockBuilder::new();
        build(&mut b);
        b.take()
            .expect("encode fixture")
            .expect("fixture has elements")
            .to_vec()
    };
    vec![
        (
            "plain and dense nodes in one group, IDs out of order",
            block(
                &strings,
                &[Group {
                    nodes: vec![
                        plain_node(5, 100, -100, &[(1, 2)], Some([2, 7, 9, 3, 3])),
                        plain_node(2, -50, 70, &[], None),
                    ],
                    dense: Some(dense(
                        &[(10, 1, 1), (3, 2, -2), (-4, 0, 0)],
                        &[1, 2, 0, 0, 0],
                    )),
                    ..Group::default()
                }],
            ),
        ),
        (
            "several groups of every kind",
            block(
                &strings,
                &[
                    Group {
                        dense: Some(dense(&[(7, 5, 5), (8, 6, 6)], &[])),
                        ..Group::default()
                    },
                    Group {
                        nodes: vec![plain_node(1, 9, 9, &[(1, 2)], None)],
                        ways: vec![way(20, &[7, 8, 1], (1, 2)), way(21, &[-3, 9], (1, 2))],
                        ..Group::default()
                    },
                    Group {
                        relations: vec![
                            relation(30, &[(3, 8, 0), (3, 21, 1), (0, 30, 2)]),
                            relation(31, &[(3, 5, 7), (3, 99, 0)]),
                        ],
                        ..Group::default()
                    },
                    Group::default(),
                ],
            ),
        ),
        (
            "ways and relations sharing a group, then plain and dense nodes",
            block(
                &strings,
                &[
                    Group {
                        ways: vec![way(40, &[1, -2, 3], (1, 2)), way(-41, &[5], (1, 2))],
                        relations: vec![
                            relation(50, &[(3, 40, 1), (0, -6, 0), (3, 51, 2)]),
                            relation(51, &[(0, 7, 9)]),
                        ],
                        ..Group::default()
                    },
                    Group {
                        nodes: vec![plain_node(-9, 1, 1, &[], None)],
                        dense: Some(dense(&[(11, 1, 1), (12, 2, 2)], &[])),
                        ways: vec![way(42, &[11, 12], (1, 2))],
                        ..Group::default()
                    },
                ],
            ),
        ),
        (
            "absent packed fields: an empty dense message, a way without refs, a relation without members",
            block(
                &strings,
                &[
                    Group {
                        dense: Some(Vec::new()),
                        ..Group::default()
                    },
                    Group {
                        ways: vec![{
                            let mut w = Vec::new();
                            field_varint(&mut w, 1, 43);
                            w
                        }],
                        relations: vec![{
                            let mut r = Vec::new();
                            field_varint(&mut r, 1, 53);
                            r
                        }],
                        ..Group::default()
                    },
                ],
            ),
        ),
        (
            "builder dense nodes with metadata",
            encode(&|b| {
                for i in [5_i64, 3, 9, 9, 12, -4, 20] {
                    let tags: &[(&str, &str)] = if i % 2 == 0 { &[("k", "v")] } else { &[] };
                    b.add_node(
                        i,
                        (i * 1_000) as i32,
                        (-i * 777) as i32,
                        tags.iter().copied(),
                        Some(&meta(i)),
                    );
                }
            }),
        ),
        (
            "builder ways and relations with metadata",
            encode(&|b| {
                for i in 1..5_i64 {
                    b.add_way(i * 10, [("highway", "x")], &[1, 8, i], Some(&meta(i)));
                }
            }),
        ),
        (
            "builder relations",
            encode(&|b| {
                for i in 1..5_i64 {
                    let members = [
                        MemberData {
                            id: MemberId::Way(i * 10),
                            role: "outer",
                        },
                        MemberData {
                            id: MemberId::Node(8),
                            role: "",
                        },
                    ];
                    b.add_relation(i, [("type", "multipolygon")], &members, Some(&meta(i)));
                }
            }),
        ),
    ]
}

/// Every truncation of `bytes` and six byte values at every position, each
/// with a label.
pub(crate) fn mutations(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for len in 0..bytes.len() {
        out.push((format!("truncated to {len}"), bytes[..len].to_vec()));
    }
    for pos in 0..bytes.len() {
        for value in [0x00_u8, 0x01, 0x7f, 0x80, 0xff, bytes[pos] ^ 0x08] {
            let mut mutated = bytes.to_vec();
            mutated[pos] = value;
            out.push((format!("byte {pos} = {value:#x}"), mutated));
        }
    }
    out
}
