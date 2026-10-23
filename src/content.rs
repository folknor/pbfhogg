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
//! `Err` token compares unequal to everything, including another `Err`.
//!
//! Changing what is content, or how [`write_record`] serializes it, changes
//! what `digest` prints: bump `digest::CANONICAL_VERSION` with it.

use crate::ElementKind;
use crate::error::Result;
use crate::osc::write::{OwnedNode, OwnedRelation, OwnedWay};
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

/// Something with element content: a borrowed [`Element`] or an owned
/// element from the merge-join's buffers.
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

impl Content for OwnedNode {
    fn content_kind(&self) -> ElementKind {
        ElementKind::Node
    }

    fn content_tokens(&self) -> impl Iterator<Item = Result<Token<'_>>> {
        std::iter::once(Token::Coord {
            lat: self.decimicro_lat,
            lon: self.decimicro_lon,
        })
        .chain(owned_tags(&self.tags))
        .map(Ok)
    }
}

impl Content for OwnedWay {
    fn content_kind(&self) -> ElementKind {
        ElementKind::Way
    }

    fn content_tokens(&self) -> impl Iterator<Item = Result<Token<'_>>> {
        owned_tags(&self.tags)
            .chain(self.refs.iter().map(|&r| Token::Ref(r)))
            .map(Ok)
    }
}

impl Content for OwnedRelation {
    fn content_kind(&self) -> ElementKind {
        ElementKind::Relation
    }

    fn content_tokens(&self) -> impl Iterator<Item = Result<Token<'_>>> {
        owned_tags(&self.tags)
            .chain(self.members.iter().map(|m| Token::Member {
                id: m.id,
                role: m.role.as_str(),
            }))
            .map(Ok)
    }
}

fn owned_tags(tags: &[(String, String)]) -> impl Iterator<Item = Token<'_>> {
    tags.iter().map(|(k, v)| Token::Tag(k.as_str(), v.as_str()))
}

// ---------------------------------------------------------------------------
// Canonical serialization
// ---------------------------------------------------------------------------

// Record framing. Every discriminant is distinct, so a record stream stays
// uniquely parseable once block boundaries are gone: a record is a kind byte,
// the id, type-tagged tokens, and RECORD_END.
const KIND_NODE: u8 = 0x01;
const KIND_WAY: u8 = 0x02;
const KIND_RELATION: u8 = 0x03;
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
            Token::Tag(key, value) => {
                out.push(TOKEN_TAG);
                write_str(out, key)?;
                write_str(out, value)?;
            }
            Token::Ref(r) => {
                out.push(TOKEN_REF);
                out.extend_from_slice(&r.to_le_bytes());
            }
            Token::Member { id, role } => {
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
                write_str(out, role)?;
            }
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
