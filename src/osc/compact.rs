//! Arena-packed binary layout for OSC elements, plus the zero-copy iterator
//! and accessor types that read them back out. Tag keys and relation member
//! roles are stored as u32 intern IDs into the `StringInterner` owned by the
//! enclosing [`super::parse::CompactDiffOverlay`].

use crate::read::elements::MemberType;

use super::interner::StringInterner;

// ---------------------------------------------------------------------------
// LE byte-reading helpers
// ---------------------------------------------------------------------------

#[inline]
fn read_i64_le(data: &[u8], offset: usize) -> i64 {
    let bytes: [u8; 8] = data[offset..offset + 8].try_into().expect("slice length");
    i64::from_le_bytes(bytes)
}

#[inline]
fn read_i32_le(data: &[u8], offset: usize) -> i32 {
    let bytes: [u8; 4] = data[offset..offset + 4].try_into().expect("slice length");
    i32::from_le_bytes(bytes)
}

#[inline]
fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    let bytes: [u8; 4] = data[offset..offset + 4].try_into().expect("slice length");
    u32::from_le_bytes(bytes)
}

// ---------------------------------------------------------------------------
// MemberType <-> byte conversion
// ---------------------------------------------------------------------------

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn member_type_to_byte(mt: MemberType) -> u8 {
    match mt {
        MemberType::Node => 0,
        MemberType::Way => 1,
        MemberType::Relation => 2,
        MemberType::Unknown(v) => v as u8,
    }
}

fn byte_to_member_type(b: u8) -> MemberType {
    match b {
        0 => MemberType::Node,
        1 => MemberType::Way,
        2 => MemberType::Relation,
        other => MemberType::Unknown(i32::from(other)),
    }
}

// ---------------------------------------------------------------------------
// Arena append functions
// ---------------------------------------------------------------------------

/// Append a node to the arena in packed binary layout.
///
/// Layout: `[id:i64 LE][lat:i32 LE][lon:i32 LE][tag_count:u32 LE]`
/// then per tag: `[key_intern_id:u32 LE][value_len:u32 LE][value_bytes]`
///
/// Returns the byte offset where this node starts in the arena.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn arena_append_node(
    arena: &mut Vec<u8>,
    id: i64,
    lat: i32,
    lon: i32,
    tags: &[(u32, &str)],
) -> u32 {
    let offset = arena.len() as u32;
    arena.extend_from_slice(&id.to_le_bytes());
    arena.extend_from_slice(&lat.to_le_bytes());
    arena.extend_from_slice(&lon.to_le_bytes());
    let tag_count = tags.len() as u32;
    arena.extend_from_slice(&tag_count.to_le_bytes());
    for &(key_id, value) in tags {
        arena.extend_from_slice(&key_id.to_le_bytes());
        let value_len = value.len() as u32;
        arena.extend_from_slice(&value_len.to_le_bytes());
        arena.extend_from_slice(value.as_bytes());
    }
    offset
}

/// Append a way to the arena in packed binary layout.
///
/// Layout: `[id:i64 LE][ref_count:u32 LE][tag_count:u32 LE]`
/// then `ref_count` x `[ref_id:i64 LE]`, then tags (same format as nodes).
///
/// Returns the byte offset where this way starts in the arena.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn arena_append_way(
    arena: &mut Vec<u8>,
    id: i64,
    refs: &[i64],
    tags: &[(u32, &str)],
) -> u32 {
    let offset = arena.len() as u32;
    arena.extend_from_slice(&id.to_le_bytes());
    let ref_count = refs.len() as u32;
    arena.extend_from_slice(&ref_count.to_le_bytes());
    let tag_count = tags.len() as u32;
    arena.extend_from_slice(&tag_count.to_le_bytes());
    for &r in refs {
        arena.extend_from_slice(&r.to_le_bytes());
    }
    for &(key_id, value) in tags {
        arena.extend_from_slice(&key_id.to_le_bytes());
        let value_len = value.len() as u32;
        arena.extend_from_slice(&value_len.to_le_bytes());
        arena.extend_from_slice(value.as_bytes());
    }
    offset
}

/// Append a relation to the arena in packed binary layout.
///
/// Layout: `[id:i64 LE][member_count:u32 LE][tag_count:u32 LE]`
/// then per member: `[ref_id:i64 LE][type:u8][role_intern_id:u32 LE]` (13 bytes each),
/// then tags (same format as nodes).
///
/// Returns the byte offset where this relation starts in the arena.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn arena_append_relation(
    arena: &mut Vec<u8>,
    id: i64,
    members: &[(i64, u8, u32)],
    tags: &[(u32, &str)],
) -> u32 {
    let offset = arena.len() as u32;
    arena.extend_from_slice(&id.to_le_bytes());
    let member_count = members.len() as u32;
    arena.extend_from_slice(&member_count.to_le_bytes());
    let tag_count = tags.len() as u32;
    arena.extend_from_slice(&tag_count.to_le_bytes());
    for &(ref_id, type_byte, role_id) in members {
        arena.extend_from_slice(&ref_id.to_le_bytes());
        arena.push(type_byte);
        arena.extend_from_slice(&role_id.to_le_bytes());
    }
    for &(key_id, value) in tags {
        arena.extend_from_slice(&key_id.to_le_bytes());
        let value_len = value.len() as u32;
        arena.extend_from_slice(&value_len.to_le_bytes());
        arena.extend_from_slice(value.as_bytes());
    }
    offset
}

// ---------------------------------------------------------------------------
// Iterator types
// ---------------------------------------------------------------------------

/// Iterator over tags in arena-packed binary layout.
///
/// Each tag is stored as `[key_intern_id:u32 LE][value_len:u32 LE][value_bytes]`.
/// Yields `(&str, &str)` pairs of (key, value).
pub struct CompactTagIter<'a> {
    data: &'a [u8],
    offset: usize,
    remaining: usize,
    interner: &'a StringInterner,
}

impl<'a> Iterator for CompactTagIter<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let key_id = read_u32_le(self.data, self.offset);
        self.offset += 4;
        let value_len = read_u32_le(self.data, self.offset) as usize;
        self.offset += 4;
        let value_bytes = &self.data[self.offset..self.offset + value_len];
        self.offset += value_len;
        let key = self.interner.resolve(key_id);
        let value = std::str::from_utf8(value_bytes).unwrap_or("");
        Some((key, value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for CompactTagIter<'_> {}

/// Iterator over way node references in arena-packed binary layout.
///
/// Each ref is stored as `[ref_id:i64 LE]`. Yields `i64` values.
pub struct CompactRefIter<'a> {
    data: &'a [u8],
    offset: usize,
    remaining: usize,
}

impl Iterator for CompactRefIter<'_> {
    type Item = i64;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let val = read_i64_le(self.data, self.offset);
        self.offset += 8;
        Some(val)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for CompactRefIter<'_> {}

/// Iterator over relation members in arena-packed binary layout.
///
/// Each member is `[ref_id:i64 LE][type:u8][role_intern_id:u32 LE]` (13 bytes).
/// Yields `(MemberType, i64, &str)` tuples of (type, ref_id, role).
pub struct CompactMemberIter<'a> {
    data: &'a [u8],
    offset: usize,
    remaining: usize,
    interner: &'a StringInterner,
}

impl<'a> Iterator for CompactMemberIter<'a> {
    type Item = (MemberType, i64, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let ref_id = read_i64_le(self.data, self.offset);
        self.offset += 8;
        let type_byte = self.data[self.offset];
        self.offset += 1;
        let role_id = read_u32_le(self.data, self.offset);
        self.offset += 4;
        let member_type = byte_to_member_type(type_byte);
        let role = self.interner.resolve(role_id);
        Some((member_type, ref_id, role))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for CompactMemberIter<'_> {}

// ---------------------------------------------------------------------------
// Accessor types
// ---------------------------------------------------------------------------

/// Zero-copy accessor for a node stored in the arena.
///
/// Layout: `[id:i64][lat:i32][lon:i32][tag_count:u32]` then tags.
pub struct CompactNodeRef<'a> {
    pub(super) data: &'a [u8],
    pub(super) interner: &'a StringInterner,
}

impl<'a> CompactNodeRef<'a> {
    /// Header size: 8 (id) + 4 (lat) + 4 (lon) + 4 (tag_count) = 20 bytes.
    const HEADER_LEN: usize = 20;

    /// Returns the node ID.
    pub fn id(&self) -> i64 {
        read_i64_le(self.data, 0)
    }

    /// Returns the latitude in decimicrodegrees (10^-7 degrees).
    pub fn decimicro_lat(&self) -> i32 {
        read_i32_le(self.data, 8)
    }

    /// Returns the longitude in decimicrodegrees (10^-7 degrees).
    pub fn decimicro_lon(&self) -> i32 {
        read_i32_le(self.data, 12)
    }

    /// Returns the number of tags.
    pub fn tag_count(&self) -> usize {
        read_u32_le(self.data, 16) as usize
    }

    /// Returns an iterator over the tags as `(&str, &str)` pairs.
    pub fn tags(&self) -> CompactTagIter<'a> {
        CompactTagIter {
            data: self.data,
            offset: Self::HEADER_LEN,
            remaining: self.tag_count(),
            interner: self.interner,
        }
    }
}

/// Zero-copy accessor for a way stored in the arena.
///
/// Layout: `[id:i64][ref_count:u32][tag_count:u32]` then refs, then tags.
pub struct CompactWayRef<'a> {
    pub(super) data: &'a [u8],
    pub(super) interner: &'a StringInterner,
}

impl<'a> CompactWayRef<'a> {
    /// Header size: 8 (id) + 4 (ref_count) + 4 (tag_count) = 16 bytes.
    const HEADER_LEN: usize = 16;

    /// Returns the way ID.
    pub fn id(&self) -> i64 {
        read_i64_le(self.data, 0)
    }

    /// Returns the number of node references.
    pub fn ref_count(&self) -> usize {
        read_u32_le(self.data, 8) as usize
    }

    /// Returns the number of tags.
    pub fn tag_count(&self) -> usize {
        read_u32_le(self.data, 12) as usize
    }

    /// Returns an iterator over the node references.
    pub fn refs(&self) -> CompactRefIter<'a> {
        CompactRefIter {
            data: self.data,
            offset: Self::HEADER_LEN,
            remaining: self.ref_count(),
        }
    }

    /// Returns an iterator over the tags as `(&str, &str)` pairs.
    pub fn tags(&self) -> CompactTagIter<'a> {
        let tag_offset = Self::HEADER_LEN + self.ref_count() * 8;
        CompactTagIter {
            data: self.data,
            offset: tag_offset,
            remaining: self.tag_count(),
            interner: self.interner,
        }
    }
}

/// Zero-copy accessor for a relation stored in the arena.
///
/// Layout: `[id:i64][member_count:u32][tag_count:u32]` then members, then tags.
pub struct CompactRelationRef<'a> {
    pub(super) data: &'a [u8],
    pub(super) interner: &'a StringInterner,
}

impl<'a> CompactRelationRef<'a> {
    /// Header size: 8 (id) + 4 (member_count) + 4 (tag_count) = 16 bytes.
    const HEADER_LEN: usize = 16;

    /// Per-member size: 8 (ref_id) + 1 (type) + 4 (role_intern_id) = 13 bytes.
    const MEMBER_SIZE: usize = 13;

    /// Returns the relation ID.
    pub fn id(&self) -> i64 {
        read_i64_le(self.data, 0)
    }

    /// Returns the number of members.
    pub fn member_count(&self) -> usize {
        read_u32_le(self.data, 8) as usize
    }

    /// Returns the number of tags.
    pub fn tag_count(&self) -> usize {
        read_u32_le(self.data, 12) as usize
    }

    /// Returns an iterator over the members as `(MemberType, i64, &str)` tuples.
    pub fn members(&self) -> CompactMemberIter<'a> {
        CompactMemberIter {
            data: self.data,
            offset: Self::HEADER_LEN,
            remaining: self.member_count(),
            interner: self.interner,
        }
    }

    /// Returns an iterator over the tags as `(&str, &str)` pairs.
    pub fn tags(&self) -> CompactTagIter<'a> {
        let tag_offset = Self::HEADER_LEN + self.member_count() * Self::MEMBER_SIZE;
        CompactTagIter {
            data: self.data,
            offset: tag_offset,
            remaining: self.tag_count(),
            interner: self.interner,
        }
    }
}
