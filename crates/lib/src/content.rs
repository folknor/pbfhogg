//! Element content: the single definition of what two elements with the same
//! kind and ID are compared on.
//!
//! `diff` (text and `--format osc`, every path) decides "common" versus
//! "modified" with [`content_equal`], and `digest` hashes the same token
//! stream through [`write_record`]. Nothing else in the crate may define
//! element equality; a second definition is how the two drift apart, and a
//! digest that drifts from diff turns a cached verification into a false
//! pass.
//!
//! Content, per kind:
//!
//! - **node:** latitude and longitude in decimicrodegrees (10^-7 degrees, the
//!   resolution every reader and writer here agrees on, so dense versus
//!   plain encoding and granularity/offset choices do not matter), then the
//!   tags in stored order.
//! - **way:** the tags in stored order, then the node refs in order.
//!   `LocationsOnWays` coordinates are not content: they are derived data.
//! - **relation:** the tags in stored order, then the members in order, each
//!   as (member id including its type, role).
//!
//! Never content: metadata of any kind (version, timestamp, changeset, uid,
//! user, `visible`), header fields, and anything about the encoding.
//!
//! Every string is resolved through the fallible string-table lookup, so a
//! bad index is an `Err` token, never a silently shortened tag list. An
//! `Err` token the comparison reaches compares unequal to everything,
//! including another `Err`; a comparison that already differs on an
//! earlier token stops before it.
//!
//! Diff and derive validate string indices in exactly three places, the
//! same on every path:
//!
//! - **Comparison** sees only content tokens: a bad tag or role index it
//!   reaches makes the pair unequal. Metadata (the user name) is not
//!   content, so a bad user index never affects equality.
//! - **Derive's OSC writers** resolve every string they print - tags,
//!   roles and the user name - through the checked lookups and refuse a bad
//!   index, so a created or modified element is never printed shortened.
//!   A delete prints only id and version and validates nothing else.
//! - **Diff's verbose details** for a modified pair resolve the tags and
//!   roles they print through the checked lookups. The compact text lines
//!   print only kind, id and version.
//!
//! Anything else - a bad index in an element only deleted, created in a
//! text diff, or in a pair decided unequal before the bad token - is not
//! validated by diff or derive; that is `check`'s job. The indexed
//! byte-equal blob fast path does not decode at all.
//!
//! Changing what is content, or how [`write_record`] serializes it, changes
//! what `digest --form content` prints: give `digest::Form::Content` a new,
//! never-used version number with it.
//!
//! # The full form
//!
//! `digest --form full` hashes a second, stricter record per element,
//! [`write_full_record`], and a header record, [`write_header_record`]. It
//! is not an equality diff computes: it is the equality "same header, same
//! elements, field for field", for checking that two writers of the same
//! operation agree. Content as above, except that coordinates are exact
//! nanodegrees, plus:
//!
//! - **metadata**, as [`MetaClaims`] reads it (the reading every metadata
//!   output shares): each of version, millisecond timestamp, changeset, uid,
//!   user and `visible`, each claimed or not. A claim is what the reading
//!   says, not the raw wire: a version below 1 is never claimed, and a dense
//!   versionless slot claims only values padding cannot hold. Where a plain
//!   `Info` carries a field, an absent one and a present zero, empty or
//!   `true` one differ;
//! - **way coordinates** (`LocationsOnWays`), in ref order, wherever a way
//!   carries them;
//! - **the prepass fields**, only where the header declares them (they are
//!   authoritative nowhere else, ADR-0007): a way's
//!   `pbfhogg.SharedNodePins-v1` pin positions and its
//!   `pbfhogg.WayMembers-v1` membership bit, each normalised to the way, so
//!   blob boundaries do not enter;
//! - **the header**: bbox, required and optional features (each as a set),
//!   writing program, source and the three replication fields, each with
//!   its presence.
//!
//! Still not in it: compression, blob boundaries, dense versus plain
//! encoding of a node (beyond what the two metadata readings can tell
//! apart), string-table order, granularity, offsets, date granularity,
//! indexdata, and undeclared prepass bytes. Changing any of this, or the
//! serialization, gives `digest::Form::Full` a new version number.

use std::collections::BTreeSet;

use crate::ElementKind;
use crate::HeaderBlock;
use crate::error::{Error, Result};
use crate::read::claims::MetaClaims;
use crate::{Element, MemberId};

/// One unit of element content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Token<'a> {
    /// A node's location, decimicrodegrees.
    Coord {
        lat: i32,
        lon: i32,
    },
    Tag(&'a str, &'a str),
    /// A way's node ref.
    Ref(i64),
    /// A relation member.
    Member {
        id: MemberId,
        role: &'a str,
    },
}

/// Something with element content. Only borrowed [`Element`]s implement it:
/// every diff and derive path compares elements straight from their
/// decoded blocks.
pub(crate) trait Content {
    fn content_kind(&self) -> ElementKind;
    fn content_tokens(&self) -> impl Iterator<Item = Result<Token<'_>>>;
}

/// Content equality, the relation `diff` reports as "common".
///
/// Elements of different kinds are never equal. A lookup failure on either
/// side makes the pair unequal; it never makes corrupt input look identical.
pub(crate) fn content_equal(a: &impl Content, b: &impl Content) -> bool {
    if a.content_kind() != b.content_kind() {
        return false;
    }
    let mut a_tokens = a.content_tokens();
    let mut b_tokens = b.content_tokens();
    loop {
        match (a_tokens.next(), b_tokens.next()) {
            (None, None) => return true,
            (Some(Ok(x)), Some(Ok(y))) if x == y => {}
            _ => return false,
        }
    }
}

impl Content for Element<'_> {
    fn content_kind(&self) -> ElementKind {
        match self {
            Element::DenseNode(_) | Element::Node(_) => ElementKind::Node,
            Element::Way(_) => ElementKind::Way,
            Element::Relation(_) => ElementKind::Relation,
        }
    }

    fn content_tokens(&self) -> impl Iterator<Item = Result<Token<'_>>> {
        ElementTokens::new(self)
    }
}

/// Token stream over a borrowed element. An enum rather than boxed
/// iterators: this runs for every compared element in a planet diff.
enum ElementTokens<'a> {
    DenseNode {
        coord: Option<Token<'a>>,
        tags: crate::CheckedDenseTagIter<'a>,
    },
    Node {
        coord: Option<Token<'a>>,
        tags: crate::CheckedTagIter<'a>,
    },
    Way {
        tags: crate::CheckedTagIter<'a>,
        refs: crate::WayRefIter<'a>,
    },
    Relation {
        tags: crate::CheckedTagIter<'a>,
        members: crate::RelMemberIter<'a>,
    },
}

impl<'a> ElementTokens<'a> {
    fn new(element: &Element<'a>) -> Self {
        match element {
            Element::DenseNode(n) => ElementTokens::DenseNode {
                coord: Some(Token::Coord {
                    lat: n.decimicro_lat(),
                    lon: n.decimicro_lon(),
                }),
                tags: n.tags_checked(),
            },
            Element::Node(n) => ElementTokens::Node {
                coord: Some(Token::Coord {
                    lat: n.decimicro_lat(),
                    lon: n.decimicro_lon(),
                }),
                tags: n.tags_checked(),
            },
            Element::Way(w) => ElementTokens::Way {
                tags: w.tags_checked(),
                refs: w.refs(),
            },
            Element::Relation(r) => ElementTokens::Relation {
                tags: r.tags_checked(),
                members: r.members(),
            },
        }
    }
}

fn tag_token<'a>(tag: Result<(&'a str, &'a str)>) -> Result<Token<'a>> {
    tag.map(|(k, v)| Token::Tag(k, v))
}

impl<'a> Iterator for ElementTokens<'a> {
    type Item = Result<Token<'a>>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ElementTokens::DenseNode { coord, tags } => {
                if let Some(c) = coord.take() {
                    return Some(Ok(c));
                }
                tags.next().map(tag_token)
            }
            ElementTokens::Node { coord, tags } => {
                if let Some(c) = coord.take() {
                    return Some(Ok(c));
                }
                tags.next().map(tag_token)
            }
            ElementTokens::Way { tags, refs } => match tags.next() {
                Some(tag) => Some(tag_token(tag)),
                None => refs.next().map(|r| Ok(Token::Ref(r))),
            },
            ElementTokens::Relation { tags, members } => match tags.next() {
                Some(tag) => Some(tag_token(tag)),
                None => members.next().map(|m| {
                    let id = m.id;
                    m.role().map(|role| Token::Member { id, role })
                }),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Canonical serialization
// ---------------------------------------------------------------------------

// Record framing. Every discriminant is distinct, so a record stream stays
// uniquely parseable once block boundaries are gone: a record is a kind byte,
// the id, type-tagged tokens, and RECORD_END.
pub(crate) const KIND_NODE: u8 = 0x01;
pub(crate) const KIND_WAY: u8 = 0x02;
pub(crate) const KIND_RELATION: u8 = 0x03;
const TOKEN_COORD: u8 = 0x10;
const TOKEN_TAG: u8 = 0x11;
const TOKEN_REF: u8 = 0x12;
const TOKEN_MEMBER: u8 = 0x13;
const RECORD_END: u8 = 0x1F;
const MEMBER_NODE: u8 = 0x00;
const MEMBER_WAY: u8 = 0x01;
const MEMBER_RELATION: u8 = 0x02;
const MEMBER_UNKNOWN: u8 = 0x03;

/// Append one element's canonical record to `out`: kind byte, id as
/// little-endian i64, every content token, end byte. Integers are fixed-width
/// little-endian; strings are a little-endian u32 byte length then the bytes.
///
/// Fails on the first token lookup failure, leaving `out` partially written;
/// the caller discards it.
pub(crate) fn write_record(element: &Element<'_>, id: i64, out: &mut Vec<u8>) -> Result<()> {
    out.push(match element.content_kind() {
        ElementKind::Node => KIND_NODE,
        ElementKind::Way => KIND_WAY,
        ElementKind::Relation => KIND_RELATION,
    });
    out.extend_from_slice(&id.to_le_bytes());
    for token in element.content_tokens() {
        match token? {
            Token::Coord { lat, lon } => {
                out.push(TOKEN_COORD);
                out.extend_from_slice(&lat.to_le_bytes());
                out.extend_from_slice(&lon.to_le_bytes());
            }
            Token::Tag(key, value) => write_tag(out, (key, value))?,
            Token::Ref(r) => {
                out.push(TOKEN_REF);
                out.extend_from_slice(&r.to_le_bytes());
            }
            Token::Member { id, role } => write_member(out, id, role)?,
        }
    }
    out.push(RECORD_END);
    Ok(())
}

fn write_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    // A string-table entry lives inside a block capped at 32 MiB, so this
    // cannot fail on anything the reader constructed.
    let len = u32::try_from(s.len())
        .map_err(|_| crate::error::new_wire_error("string longer than u32::MAX bytes"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

// ---------------------------------------------------------------------------
// The full form
// ---------------------------------------------------------------------------

// Full-form token discriminants, distinct from every content-form one, so a
// full record stream is uniquely parseable on the same terms.
const TOKEN_NANO_COORD: u8 = 0x14;
const TOKEN_LOCATION: u8 = 0x15;
const TOKEN_PINS: u8 = 0x16;
const TOKEN_WAY_MEMBER: u8 = 0x17;
const TOKEN_META: u8 = 0x18;
const ABSENT: u8 = 0x00;
const PRESENT: u8 = 0x01;
/// Opens the header record.
const KIND_HEADER: u8 = 0x04;

/// What the header declares about the prepass fields, which decides whether
/// a way's full record carries them.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Prepass {
    /// `pbfhogg.SharedNodePins-v1` is declared.
    pub(crate) pins: bool,
}

/// Append one element's full-form record to `out`: kind byte, id, then the
/// element's tokens, its metadata token, and the end byte.
///
/// `way_member` is the way's `pbfhogg.WayMembers-v1` bit, `Some` exactly
/// when the header declares that feature (the caller reads it from the
/// blob); it is ignored for nodes and relations.
///
/// Fails on a string lookup failure, and on a declared pin bitmap that is
/// malformed for its way (a length other than one bit per ref rounded up
/// to whole bytes, or a set bit past the last ref).
pub(crate) fn write_full_record(
    element: &Element<'_>,
    id: i64,
    prepass: Prepass,
    way_member: Option<bool>,
    out: &mut Vec<u8>,
) -> Result<()> {
    out.push(match element.content_kind() {
        ElementKind::Node => KIND_NODE,
        ElementKind::Way => KIND_WAY,
        ElementKind::Relation => KIND_RELATION,
    });
    out.extend_from_slice(&id.to_le_bytes());
    let meta = match element {
        Element::DenseNode(n) => {
            write_nano_coord(out, n.nano_lat(), n.nano_lon());
            for tag in n.tags_checked() {
                write_tag(out, tag?)?;
            }
            n.info().and_then(MetaClaims::from_dense)
        }
        Element::Node(n) => {
            write_nano_coord(out, n.nano_lat(), n.nano_lon());
            for tag in n.tags_checked() {
                write_tag(out, tag?)?;
            }
            Some(MetaClaims::from_info(&n.info()))
        }
        Element::Way(w) => {
            for tag in w.tags_checked() {
                write_tag(out, tag?)?;
            }
            let mut refs = 0usize;
            for r in w.refs() {
                out.push(TOKEN_REF);
                out.extend_from_slice(&r.to_le_bytes());
                refs += 1;
            }
            for loc in w.node_locations() {
                out.push(TOKEN_LOCATION);
                out.extend_from_slice(&loc.nano_lat().to_le_bytes());
                out.extend_from_slice(&loc.nano_lon().to_le_bytes());
            }
            if prepass.pins {
                write_pins(out, w.shared_node_pins(), refs)?;
            }
            if let Some(member) = way_member {
                out.push(TOKEN_WAY_MEMBER);
                out.push(u8::from(member));
            }
            Some(MetaClaims::from_info(&w.info()))
        }
        Element::Relation(r) => {
            for tag in r.tags_checked() {
                write_tag(out, tag?)?;
            }
            for m in r.members() {
                write_member(out, m.id, m.role()?)?;
            }
            Some(MetaClaims::from_info(&r.info()))
        }
    };
    write_meta(out, meta)?;
    out.push(RECORD_END);
    Ok(())
}

fn write_nano_coord(out: &mut Vec<u8>, lat: i64, lon: i64) {
    out.push(TOKEN_NANO_COORD);
    out.extend_from_slice(&lat.to_le_bytes());
    out.extend_from_slice(&lon.to_le_bytes());
}

fn write_tag(out: &mut Vec<u8>, (key, value): (&str, &str)) -> Result<()> {
    out.push(TOKEN_TAG);
    write_str(out, key)?;
    write_str(out, value)
}

fn write_member(out: &mut Vec<u8>, id: MemberId, role: &str) -> Result<()> {
    out.push(TOKEN_MEMBER);
    match id {
        MemberId::Node(_) => out.push(MEMBER_NODE),
        MemberId::Way(_) => out.push(MEMBER_WAY),
        MemberId::Relation(_) => out.push(MEMBER_RELATION),
        MemberId::Unknown(raw, _) => {
            out.push(MEMBER_UNKNOWN);
            out.extend_from_slice(&raw.to_le_bytes());
        }
    }
    out.extend_from_slice(&id.id().to_le_bytes());
    write_str(out, role)
}

/// The pin token: the count of set positions, then each position, all u32
/// LE. An absent bitmap and an all-zero one are the same set (ADR-0007:
/// producers omit the field when no bit is set).
fn write_pins(out: &mut Vec<u8>, pins: Option<&[u8]>, refs: usize) -> Result<()> {
    let bits = pins.unwrap_or_default();
    if pins.is_some() && bits.len() != refs.div_ceil(8) {
        return Err(Error::invalid_data(format!(
            "a pbfhogg.SharedNodePins-v1 bitmap of {} byte(s) on a way of {refs} ref(s); \
             it must be one bit per ref, rounded up to whole bytes",
            bits.len()
        )));
    }
    let mut positions: Vec<u32> = Vec::new();
    for (byte_index, &byte) in bits.iter().enumerate() {
        for bit in 0..8 {
            if byte & (1 << bit) == 0 {
                continue;
            }
            let position = byte_index * 8 + bit;
            if position >= refs {
                return Err(Error::invalid_data(format!(
                    "a pbfhogg.SharedNodePins-v1 bitmap pins position {position} on a way of \
                     {refs} ref(s)"
                )));
            }
            positions.push(
                u32::try_from(position)
                    .map_err(|_| crate::error::new_wire_error("pin position beyond u32::MAX"))?,
            );
        }
    }
    out.push(TOKEN_PINS);
    write_u32_len(out, positions.len())?;
    for p in positions {
        out.extend_from_slice(&p.to_le_bytes());
    }
    Ok(())
}

/// The metadata token: each field as a presence byte, then its value when
/// present. `None` claims nothing, and serializes as every field absent.
fn write_meta(out: &mut Vec<u8>, meta: Option<MetaClaims<'_>>) -> Result<()> {
    out.push(TOKEN_META);
    let Some(m) = meta else {
        out.extend_from_slice(&[ABSENT; 6]);
        return Ok(());
    };
    write_opt(out, m.version.map(i32::to_le_bytes));
    write_opt(out, m.milli_timestamp.map(i64::to_le_bytes));
    write_opt(out, m.changeset.map(i64::to_le_bytes));
    write_opt(out, m.uid.map(i32::to_le_bytes));
    match m.user {
        None => out.push(ABSENT),
        Some(user) => {
            out.push(PRESENT);
            write_str(out, user?)?;
        }
    }
    write_opt(out, m.visible.map(|v| [u8::from(v)]));
    Ok(())
}

fn write_opt<const N: usize>(out: &mut Vec<u8>, value: Option<[u8; N]>) {
    match value {
        None => out.push(ABSENT),
        Some(bytes) => {
            out.push(PRESENT);
            out.extend_from_slice(&bytes);
        }
    }
}

fn write_opt_str(out: &mut Vec<u8>, value: Option<&str>) -> Result<()> {
    match value {
        None => out.push(ABSENT),
        Some(s) => {
            out.push(PRESENT);
            write_str(out, s)?;
        }
    }
    Ok(())
}

fn write_u32_len(out: &mut Vec<u8>, len: usize) -> Result<()> {
    let len = u32::try_from(len)
        .map_err(|_| crate::error::new_wire_error("list longer than u32::MAX entries"))?;
    out.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

/// A feature list as a set: sorted by bytes, duplicates dropped, then a
/// u32 LE count and each name.
fn write_feature_set(out: &mut Vec<u8>, features: &[String]) -> Result<()> {
    let set: BTreeSet<&str> = features.iter().map(String::as_str).collect();
    write_u32_len(out, set.len())?;
    for feature in set {
        write_str(out, feature)?;
    }
    Ok(())
}

/// Append the header's full-form record to `out`: the header kind byte,
/// then the bbox (presence, then left, right, top, bottom as stored, i64
/// LE nanodegrees), the required and the optional features (each a set),
/// writing program, source, replication timestamp, sequence number and base
/// URL (each a presence byte, then its value), and the end byte.
pub(crate) fn write_header_record(header: &HeaderBlock, out: &mut Vec<u8>) -> Result<()> {
    out.push(KIND_HEADER);
    match header.raw_bbox() {
        None => out.push(ABSENT),
        Some((left, right, top, bottom)) => {
            out.push(PRESENT);
            for v in [left, right, top, bottom] {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    write_feature_set(out, header.required_features())?;
    write_feature_set(out, header.optional_features())?;
    write_opt_str(out, header.writing_program())?;
    write_opt_str(out, header.source())?;
    write_opt(
        out,
        header.osmosis_replication_timestamp().map(i64::to_le_bytes),
    );
    write_opt(
        out,
        header
            .osmosis_replication_sequence_number()
            .map(i64::to_le_bytes),
    );
    write_opt_str(out, header.osmosis_replication_base_url())?;
    out.push(RECORD_END);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::write_pins;

    fn pins(bits: Option<&[u8]>, refs: usize) -> crate::Result<Vec<u8>> {
        let mut out = Vec::new();
        write_pins(&mut out, bits, refs).map(|()| out)
    }

    /// Absent and all-zero are the same set; positions are LSB-first.
    #[test]
    fn pins_normalise_to_set_positions() {
        assert_eq!(pins(None, 9).unwrap(), pins(Some(&[0, 0]), 9).unwrap());
        let one = pins(Some(&[0b0000_0010, 0b0000_0001]), 9).unwrap();
        let mut want = vec![super::TOKEN_PINS];
        want.extend_from_slice(&2u32.to_le_bytes());
        want.extend_from_slice(&1u32.to_le_bytes());
        want.extend_from_slice(&8u32.to_le_bytes());
        assert_eq!(one, want);
    }

    /// A bitmap of the wrong width, or one pinning a position past the last
    /// ref, is refused rather than normalised.
    #[test]
    fn malformed_pins_are_refused() {
        // Nine refs need two bytes: a short and a long bitmap.
        assert!(pins(Some(&[0b0000_0001]), 9).is_err());
        assert!(pins(Some(&[1, 0, 0]), 9).is_err());
        // Bit 9 on a nine-ref way.
        assert!(pins(Some(&[0, 0b0000_0010]), 9).is_err());
        // An empty present bitmap on a way with refs.
        assert!(pins(Some(&[]), 3).is_err());
        assert!(pins(Some(&[]), 0).is_ok());
    }
}
