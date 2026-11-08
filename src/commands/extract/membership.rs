//! Shared region membership for single-pass multi-extract.
//!
//! One [`Membership`] per element kind answers "which of the N regions hold
//! this ID" with one lookup, in place of N independent `IdSet`s. N `IdSet`s
//! cost N bits per position of the ID space wherever their chunks are dense,
//! and at planet they are dense everywhere: OSM IDs are chronological, not
//! geographic, so every region's members spread over the whole ID history.
//! Planet on bygg, five disjoint strips inside `-10,35,32,72`: five node sets
//! of 406 dense chunks each, 8.5 GB, essentially the command's whole peak,
//! for 3.4e9 members over 13.6e9 positions.
//!
//! # Representation
//!
//! The ID space is cut into pages of 4096 IDs; 8192 pages make a chunk of
//! 2^25 IDs, addressed as `IdSet` addresses its chunks (directly below 2^43,
//! an ordered map above, so a near-`i64::MAX` ID costs no allocation
//! proportional to it). A chunk holds a sorted page list while it has few
//! pages and an 8192-slot table after. An absent page has no members; a
//! present one has at least one, and every member a nonzero region mask
//! whose bits at and above N are zero. A page is in one of two forms:
//!
//! - **Compact**: the members (a sorted `u16` offset list, or a 4096-bit
//!   union bitmap with per-512-bit rank prefixes), a palette of the distinct
//!   masks on the page, and one packed palette index per member, in member
//!   order, of the width the palette needs. A page whose members share one
//!   mask stores no indices at all. Five disjoint regions put up to five
//!   singleton masks on a page, three bits per member: at union density
//!   0.25 that is about 1.75 bits per ID position instead of 5.
//! - **Incidence**: per region present on the page, that region's offsets
//!   (a short sorted list, then a 64-word bitmap) - an `IdSet` chunk in
//!   miniature, at `u16` per member. This is the mutable form, and it is
//!   never larger than N `IdSet`s over the same page.
//!
//! Every encoding picks whichever form is smaller for the page's contents.
//!
//! # Building
//!
//! Nothing reads a kind's membership while it is built (nodes in node
//! classify, ways in way classify, relations in relation classify), so the
//! builder may be order-independent and unsorted. Workers turn one blob's
//! matches into encoded pages ([`BatchBuilder`]); the consumer moves a page
//! into an empty slot, or merges it into the occupied one by OR of masks
//! ([`Membership::install`]). Membership is therefore the OR, over every
//! occurrence of an ID, of the regions that occurrence classified into,
//! whatever the arrival order, duplicates included - what N `IdSet`s held.
//!
//! On honestly sorted input only a page that two blobs share is ever merged.
//! A page that keeps being revisited (duplicate IDs, a false sort claim,
//! tiny blobs) takes at most `MEMBERSHIP_MERGE_BUDGET` whole-page merges and
//! then turns, one way, into its Incidence form, where each further
//! ID-region incidence costs one scan of the regions present on the page
//! (at most N, normally a handful) and an insert into that region's part.
//! That scan is the trade against N `IdSet`s, which index a region directly:
//! a page that gains one incidence for each of N distinct regions does
//! O(N^2) region comparisons, and nothing caps N. [`Membership::freeze`]
//! re-chooses the smaller form for every page left in Incidence form.
//!
//! Negative IDs are not members and are dropped before any arithmetic, as
//! `IdSet::set` drops them (decisions/0002-negative-ids-rejected-project-wide.md).

// Offsets are masked to 12 bits, page-in-chunk indices to 13, region
// numbers are below N; every narrowing cast is bounded by construction.
#![allow(clippy::cast_possible_truncation)]

use std::collections::{BTreeMap, HashMap};

use crate::tunables::{
    MEMBERSHIP_MERGE_BUDGET, MEMBERSHIP_PART_SPARSE_MAX, MEMBERSHIP_SPARSE_MEMBERS_MAX,
    MEMBERSHIP_SPARSE_TABLE_MAX,
};

/// IDs per page = bits of a page's union bitmap.
const PAGE_SHIFT: u32 = 12;
const PAGE_IDS: usize = 1 << PAGE_SHIFT;
const PAGE_WORDS: usize = PAGE_IDS / 64;
/// Pages per chunk; a chunk is 2^25 IDs, `IdSet`'s chunk.
const TABLE_SHIFT: u32 = 13;
const TABLE_PAGES: usize = 1 << TABLE_SHIFT;
/// Chunk ids below this are indexed directly (IDs below 2^43).
const DIRECT_CHUNKS: u64 = 1 << 18;
/// Rank block of a dense page's union bitmap: 8 words, 512 bits.
const RANK_BLOCK_WORDS: usize = 8;

/// Words in one region mask over `regions` regions.
pub(super) fn mask_words(regions: usize) -> usize {
    regions.div_ceil(64).max(1)
}

/// The mask with every one of `regions` regions set.
pub(super) fn full_mask(regions: usize) -> Vec<u64> {
    let mut mask = vec![!0u64; mask_words(regions)];
    let tail = regions % 64;
    if tail != 0
        && let Some(last) = mask.last_mut()
    {
        *last = (1u64 << tail) - 1;
    }
    if regions == 0 {
        mask.fill(0);
    }
    mask
}

/// The regions set in `mask`, ascending.
pub(super) fn regions_of(mask: &[u64]) -> impl Iterator<Item = usize> + '_ {
    mask.iter().enumerate().flat_map(|(w, &word)| {
        let mut bits = word;
        std::iter::from_fn(move || {
            if bits == 0 {
                return None;
            }
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            Some(w * 64 + bit)
        })
    })
}

/// Split a non-negative ID into (page key, offset in page).
#[inline]
fn split(id: i64) -> Option<(u64, u16)> {
    let id = u64::try_from(id).ok()?;
    Some((id >> PAGE_SHIFT, (id & (PAGE_IDS as u64 - 1)) as u16))
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

/// A page's members with their masks, sorted by offset, unique: the form
/// every encoding starts from and every merge decodes to.
#[derive(Default)]
struct Entries {
    offsets: Vec<u16>,
    /// `mask_words` words per entry.
    masks: Vec<u64>,
}

impl Entries {
    fn clear(&mut self) {
        self.offsets.clear();
        self.masks.clear();
    }

    fn mask(&self, i: usize, mw: usize) -> &[u64] {
        &self.masks[i * mw..(i + 1) * mw]
    }
}

enum Page {
    Compact(Compact),
    Incidence(Incidence),
}

/// The frozen form. `data` holds, in order: the members (64 bitmap words if
/// `dense`, else `members` offsets packed four to a word), the palette
/// (`palette` masks of `mask_words` words) and the packed indices.
struct Compact {
    data: Box<[u64]>,
    /// Members before each 512-bit rank block (dense only).
    prefix: [u16; PAGE_WORDS / RANK_BLOCK_WORDS],
    members: u16,
    palette: u16,
    width: u8,
    dense: bool,
    /// Whole-page merges this page has taken.
    merges: u8,
}

/// The mutable form: per region present, that region's offsets.
struct Incidence {
    parts: Vec<(u32, Part)>,
    /// Changed since an encoding last chose this form; only such a page is
    /// re-chosen at freeze.
    mutated: bool,
}

enum Part {
    Sparse(Vec<u16>),
    Bits(Box<[u64; PAGE_WORDS]>),
}

impl Part {
    fn contains(&self, off: u16) -> bool {
        match self {
            Self::Sparse(offs) => offs.binary_search(&off).is_ok(),
            Self::Bits(bits) => bits[usize::from(off >> 6)] & (1u64 << (off & 63)) != 0,
        }
    }

    fn insert(&mut self, off: u16) {
        match self {
            Self::Sparse(offs) => {
                if let Err(at) = offs.binary_search(&off) {
                    offs.insert(at, off);
                    if offs.len() > MEMBERSHIP_PART_SPARSE_MAX {
                        let mut bits = Box::new([0u64; PAGE_WORDS]);
                        for &o in offs.iter() {
                            bits[usize::from(o >> 6)] |= 1u64 << (o & 63);
                        }
                        *self = Self::Bits(bits);
                    }
                }
            }
            Self::Bits(bits) => bits[usize::from(off >> 6)] |= 1u64 << (off & 63),
        }
    }
}

/// Bytes of a `u16` offset list or bitmap of `count` members, as a part.
fn part_bytes(count: usize) -> usize {
    if count <= MEMBERSHIP_PART_SPARSE_MAX {
        count * 2
    } else {
        PAGE_WORDS * 8
    }
}

/// Per-entry bookkeeping charged to each region a page's Incidence form
/// holds (the `(u32, Part)` slot).
const PART_OVERHEAD: usize = std::mem::size_of::<(u32, Part)>();

impl Compact {
    fn members_words(&self) -> usize {
        if self.dense {
            PAGE_WORDS
        } else {
            usize::from(self.members).div_ceil(4)
        }
    }

    fn sparse_offset(&self, i: usize) -> u16 {
        (self.data[i / 4] >> (16 * (i % 4))) as u16
    }

    /// Rank of `off` among the members, if it is one.
    #[inline]
    fn rank(&self, off: u16) -> Option<usize> {
        if self.dense {
            let w = usize::from(off >> 6);
            let bit = 1u64 << (off & 63);
            let word = self.data[w];
            if word & bit == 0 {
                return None;
            }
            let block = w / RANK_BLOCK_WORDS;
            let mut rank = usize::from(self.prefix[block]);
            for &prior in &self.data[block * RANK_BLOCK_WORDS..w] {
                rank += prior.count_ones() as usize;
            }
            Some(rank + (word & (bit - 1)).count_ones() as usize)
        } else {
            let (mut lo, mut hi) = (0usize, usize::from(self.members));
            while lo < hi {
                let mid = usize::midpoint(lo, hi);
                let at = self.sparse_offset(mid);
                if at == off {
                    return Some(mid);
                }
                if at < off {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            None
        }
    }

    fn index(&self, rank: usize, mw: usize) -> usize {
        if self.width == 0 {
            return 0;
        }
        let width = usize::from(self.width);
        let base = self.members_words() + usize::from(self.palette) * mw;
        let bit = rank * width;
        let (w, shift) = (base + bit / 64, bit % 64);
        let mut value = self.data[w] >> shift;
        if shift + width > 64 {
            value |= self.data[w + 1] << (64 - shift);
        }
        (value & ((1u64 << width) - 1)) as usize
    }

    fn palette_mask(&self, index: usize, mw: usize) -> &[u64] {
        let start = self.members_words() + index * mw;
        &self.data[start..start + mw]
    }

    #[inline]
    fn or_into(&self, off: u16, acc: &mut [u64]) -> bool {
        let mw = acc.len();
        let Some(rank) = self.rank(off) else {
            return false;
        };
        let mask = self.palette_mask(self.index(rank, mw), mw);
        for (a, m) in acc.iter_mut().zip(mask) {
            *a |= m;
        }
        true
    }

    fn decode_into(&self, mw: usize, out: &mut Entries) {
        out.clear();
        let push = |rank: usize, off: u16, out: &mut Entries| {
            out.offsets.push(off);
            let index = self.index(rank, mw);
            out.masks.extend_from_slice(self.palette_mask(index, mw));
        };
        if self.dense {
            let mut rank = 0;
            for (w, &word) in self.data[..PAGE_WORDS].iter().enumerate() {
                let mut bits = word;
                while bits != 0 {
                    let off = (w * 64) as u16 + bits.trailing_zeros() as u16;
                    bits &= bits - 1;
                    push(rank, off, out);
                    rank += 1;
                }
            }
        } else {
            for rank in 0..usize::from(self.members) {
                push(rank, self.sparse_offset(rank), out);
            }
        }
    }
}

impl Incidence {
    fn or_into(&self, off: u16, acc: &mut [u64]) -> bool {
        let mut hit = false;
        for (region, part) in &self.parts {
            if part.contains(off) {
                acc[*region as usize / 64] |= 1u64 << (*region % 64);
                hit = true;
            }
        }
        hit
    }

    /// Add every region of `mask` at `off`.
    fn add(&mut self, off: u16, mask: &[u64]) {
        for region in regions_of(mask) {
            let region = region as u32;
            match self.parts.iter_mut().find(|(r, _)| *r == region) {
                Some((_, part)) => part.insert(off),
                None => self.parts.push((region, Part::Sparse(vec![off]))),
            }
        }
    }

    fn decode_into(&self, mw: usize, out: &mut Entries) {
        out.clear();
        let mut pairs: Vec<(u16, u32)> = Vec::new();
        for (region, part) in &self.parts {
            match part {
                Part::Sparse(offs) => pairs.extend(offs.iter().map(|&o| (o, *region))),
                Part::Bits(bits) => {
                    for (w, &word) in bits.iter().enumerate() {
                        let mut b = word;
                        while b != 0 {
                            let off = (w * 64) as u16 + b.trailing_zeros() as u16;
                            b &= b - 1;
                            pairs.push((off, *region));
                        }
                    }
                }
            }
        }
        pairs.sort_unstable();
        for (off, region) in pairs {
            if out.offsets.last() != Some(&off) {
                out.offsets.push(off);
                out.masks.extend(std::iter::repeat_n(0, mw));
            }
            let at = out.masks.len() - mw;
            out.masks[at + region as usize / 64] |= 1u64 << (region % 64);
        }
    }
}

impl Page {
    #[inline]
    fn or_into(&self, off: u16, acc: &mut [u64]) -> bool {
        match self {
            Self::Compact(c) => c.or_into(off, acc),
            Self::Incidence(i) => i.or_into(off, acc),
        }
    }

    fn decode_into(&self, mw: usize, out: &mut Entries) {
        match self {
            Self::Compact(c) => c.decode_into(mw, out),
            Self::Incidence(i) => i.decode_into(mw, out),
        }
    }
}

/// The distinct masks of `entries` in first-seen order (as the entry index
/// that first carries each), and each entry's palette index. A short palette
/// is searched linearly; past that a map over the entries' own masks keeps a
/// page of thousands of distinct masks linear to encode.
fn build_palette(entries: &Entries, mw: usize) -> (Vec<usize>, Vec<u32>) {
    let count = entries.offsets.len();
    let mut palette_first: Vec<usize> = Vec::new();
    let mut indices: Vec<u32> = Vec::with_capacity(count);
    let mut map: Option<HashMap<&[u64], u32>> = None;
    for i in 0..count {
        let mask = entries.mask(i, mw);
        // Runs of one mask are the common case: try the previous entry's.
        if let Some(&last) = indices.last()
            && entries.mask(palette_first[last as usize], mw) == mask
        {
            indices.push(last);
            continue;
        }
        let found = match &map {
            Some(map) => map.get(mask).copied(),
            None => palette_first
                .iter()
                .position(|&first| entries.mask(first, mw) == mask)
                .map(|p| p as u32),
        };
        let index = found.unwrap_or_else(|| {
            let index = palette_first.len() as u32;
            palette_first.push(i);
            if let Some(map) = &mut map {
                map.insert(mask, index);
            } else if palette_first.len() > 16 {
                map = Some(
                    palette_first
                        .iter()
                        .enumerate()
                        .map(|(p, &first)| (entries.mask(first, mw), p as u32))
                        .collect(),
                );
            }
            index
        });
        indices.push(index);
    }
    (palette_first, indices)
}

/// The Incidence form of `entries`, with each region's member count.
fn incidence_of(entries: &Entries, mw: usize, per_region: &BTreeMap<usize, usize>) -> Page {
    let mut incidence = Incidence {
        parts: Vec::with_capacity(per_region.len()),
        mutated: false,
    };
    for &region in per_region.keys() {
        incidence
            .parts
            .push((region as u32, Part::Sparse(Vec::new())));
    }
    for i in 0..entries.offsets.len() {
        incidence.add(entries.offsets[i], entries.mask(i, mw));
    }
    for (_, part) in &mut incidence.parts {
        if let Part::Sparse(offs) = part {
            offs.shrink_to_fit();
        }
    }
    Page::Incidence(incidence)
}

/// Encode a non-empty page, choosing the smaller form.
fn encode(entries: &Entries, mw: usize, merges: u8) -> Page {
    let count = entries.offsets.len();
    let (palette_first, indices) = build_palette(entries, mw);
    let palette = palette_first.len();
    let width = if palette <= 1 {
        0
    } else {
        64 - ((palette - 1) as u64).leading_zeros()
    };
    let dense = count > MEMBERSHIP_SPARSE_MEMBERS_MAX;
    let members_words = if dense { PAGE_WORDS } else { count.div_ceil(4) };
    let index_words = (count * width as usize).div_ceil(64);
    let compact_bytes = (members_words + palette * mw + index_words) * 8;

    // The Incidence form of the same contents, sized from the palette: each
    // region's member count is the sum of its palette entries' counts.
    let mut uses = vec![0usize; palette];
    for &index in &indices {
        uses[index as usize] += 1;
    }
    let mut per_region: BTreeMap<usize, usize> = BTreeMap::new();
    for (&first, &used) in palette_first.iter().zip(&uses) {
        for region in regions_of(entries.mask(first, mw)) {
            *per_region.entry(region).or_default() += used;
        }
    }
    let incidence_bytes: usize = per_region
        .values()
        .map(|&c| part_bytes(c) + PART_OVERHEAD)
        .sum();

    if incidence_bytes < compact_bytes {
        return incidence_of(entries, mw, &per_region);
    }

    let mut data = vec![0u64; members_words + palette * mw + index_words];
    let mut prefix = [0u16; PAGE_WORDS / RANK_BLOCK_WORDS];
    if dense {
        for &off in &entries.offsets {
            data[usize::from(off >> 6)] |= 1u64 << (off & 63);
        }
        let mut running = 0u16;
        for (block, slot) in prefix.iter_mut().enumerate() {
            *slot = running;
            for &word in &data[block * RANK_BLOCK_WORDS..(block + 1) * RANK_BLOCK_WORDS] {
                running += word.count_ones() as u16;
            }
        }
    } else {
        for (i, &off) in entries.offsets.iter().enumerate() {
            data[i / 4] |= u64::from(off) << (16 * (i % 4));
        }
    }
    for (p, &first) in palette_first.iter().enumerate() {
        let start = members_words + p * mw;
        data[start..start + mw].copy_from_slice(entries.mask(first, mw));
    }
    if width > 0 {
        let base = members_words + palette * mw;
        let width = width as usize;
        for (rank, &index) in indices.iter().enumerate() {
            let bit = rank * width;
            let (w, shift) = (base + bit / 64, bit % 64);
            data[w] |= u64::from(index) << shift;
            if shift + width > 64 {
                data[w + 1] |= u64::from(index) >> (64 - shift);
            }
        }
    }
    Page::Compact(Compact {
        data: data.into_boxed_slice(),
        prefix,
        members: count as u16,
        palette: palette as u16,
        width: width as u8,
        dense,
        merges,
    })
}

/// OR-merge two sorted entry lists into `out`.
fn merge_entries(a: &Entries, b: &Entries, mw: usize, out: &mut Entries) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < a.offsets.len() || j < b.offsets.len() {
        let take_a = j == b.offsets.len() || (i < a.offsets.len() && a.offsets[i] <= b.offsets[j]);
        let take_b = i == a.offsets.len() || (j < b.offsets.len() && b.offsets[j] <= a.offsets[i]);
        let off = if take_a { a.offsets[i] } else { b.offsets[j] };
        out.offsets.push(off);
        let at = out.masks.len();
        out.masks.extend(std::iter::repeat_n(0, mw));
        if take_a {
            for (o, m) in out.masks[at..].iter_mut().zip(a.mask(i, mw)) {
                *o |= m;
            }
            i += 1;
        }
        if take_b {
            for (o, m) in out.masks[at..].iter_mut().zip(b.mask(j, mw)) {
                *o |= m;
            }
            j += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Directory
// ---------------------------------------------------------------------------

enum Table {
    /// Sorted by page index.
    Sparse(Vec<(u16, Page)>),
    Full(Box<[Option<Page>]>),
}

impl Table {
    fn page(&self, index: u16) -> Option<&Page> {
        match self {
            Self::Sparse(pages) => pages
                .binary_search_by_key(&index, |(i, _)| *i)
                .ok()
                .map(|at| &pages[at].1),
            Self::Full(slots) => slots[usize::from(index)].as_ref(),
        }
    }

    fn pages_mut(&mut self) -> Box<dyn Iterator<Item = &mut Page> + '_> {
        match self {
            Self::Sparse(pages) => Box::new(pages.iter_mut().map(|(_, p)| p)),
            Self::Full(slots) => Box::new(slots.iter_mut().flatten()),
        }
    }
}

/// Which regions hold each ID of one element kind. See the module docs.
pub(super) struct Membership {
    mw: usize,
    direct: Vec<Option<Table>>,
    overflow: BTreeMap<u64, Table>,
    /// Merge scratch, reused across installs.
    scratch: [Entries; 3],
}

impl Membership {
    pub(super) fn new(regions: usize) -> Self {
        Self {
            mw: mask_words(regions),
            direct: Vec::new(),
            overflow: BTreeMap::new(),
            scratch: Default::default(),
        }
    }

    fn table(&self, chunk: u64) -> Option<&Table> {
        if chunk < DIRECT_CHUNKS {
            self.direct.get(chunk as usize)?.as_ref()
        } else {
            self.overflow.get(&chunk)
        }
    }

    fn table_mut(&mut self, chunk: u64) -> &mut Table {
        if chunk < DIRECT_CHUNKS {
            let at = chunk as usize;
            if self.direct.len() <= at {
                self.direct.resize_with(at + 1, || None);
            }
            self.direct[at].get_or_insert_with(|| Table::Sparse(Vec::new()))
        } else {
            self.overflow
                .entry(chunk)
                .or_insert_with(|| Table::Sparse(Vec::new()))
        }
    }

    /// OR the regions holding `id` into `acc` (`mask_words` long). Returns
    /// whether `id` is a member of any region.
    #[inline]
    pub(super) fn or_into(&self, id: i64, acc: &mut [u64]) -> bool {
        let Some((key, off)) = split(id) else {
            return false;
        };
        let Some(table) = self.table(key >> TABLE_SHIFT) else {
            return false;
        };
        let Some(page) = table.page((key & (TABLE_PAGES as u64 - 1)) as u16) else {
            return false;
        };
        page.or_into(off, acc)
    }

    /// Set `out` to the regions holding `id`, cleared on a miss. Returns
    /// whether `id` is a member of any region.
    #[inline]
    pub(super) fn mask_into(&self, id: i64, out: &mut [u64]) -> bool {
        out.fill(0);
        self.or_into(id, out)
    }

    /// Merge one blob's pages into the membership.
    pub(super) fn install(&mut self, batch: PageBatch) {
        for (key, page) in batch.0 {
            self.install_page(key, page);
        }
    }

    fn install_page(&mut self, key: u64, page: Page) {
        let mw = self.mw;
        let index = (key & (TABLE_PAGES as u64 - 1)) as u16;
        let mut scratch = std::mem::take(&mut self.scratch);
        let table = self.table_mut(key >> TABLE_SHIFT);
        let slot: &mut Page = match table {
            Table::Sparse(pages) => match pages.binary_search_by_key(&index, |(i, _)| *i) {
                Ok(at) => &mut pages[at].1,
                Err(at) => {
                    pages.insert(at, (index, page));
                    if pages.len() > MEMBERSHIP_SPARSE_TABLE_MAX {
                        let mut slots: Box<[Option<Page>]> =
                            std::iter::repeat_with(|| None).take(TABLE_PAGES).collect();
                        for (i, p) in pages.drain(..) {
                            slots[usize::from(i)] = Some(p);
                        }
                        *table = Table::Full(slots);
                    }
                    self.scratch = scratch;
                    return;
                }
            },
            Table::Full(slots) => match &mut slots[usize::from(index)] {
                Some(existing) => existing,
                empty @ None => {
                    *empty = Some(page);
                    self.scratch = scratch;
                    return;
                }
            },
        };
        merge_page(slot, &page, mw, &mut scratch);
        self.scratch = scratch;
    }

    /// Re-choose the smaller form for every page changed since an encoding
    /// chose its Incidence form.
    pub(super) fn freeze(&mut self) {
        let mw = self.mw;
        let mut entries = Entries::default();
        let tables = self
            .direct
            .iter_mut()
            .flatten()
            .chain(self.overflow.values_mut());
        for table in tables {
            for page in table.pages_mut() {
                if let Page::Incidence(incidence) = page
                    && incidence.mutated
                {
                    incidence.decode_into(mw, &mut entries);
                    *page = encode(&entries, mw, MEMBERSHIP_MERGE_BUDGET);
                }
            }
        }
    }
}

/// Merge `incoming` into `existing` by OR of masks.
fn merge_page(existing: &mut Page, incoming: &Page, mw: usize, scratch: &mut [Entries; 3]) {
    let [old, new, out] = scratch;
    incoming.decode_into(mw, new);
    match existing {
        Page::Compact(compact) if compact.merges < MEMBERSHIP_MERGE_BUDGET => {
            compact.decode_into(mw, old);
            merge_entries(old, new, mw, out);
            *existing = encode(out, mw, compact.merges + 1);
        }
        Page::Compact(compact) => {
            compact.decode_into(mw, old);
            let mut incidence = Incidence {
                parts: Vec::new(),
                mutated: true,
            };
            for i in 0..old.offsets.len() {
                incidence.add(old.offsets[i], old.mask(i, mw));
            }
            for i in 0..new.offsets.len() {
                incidence.add(new.offsets[i], new.mask(i, mw));
            }
            *existing = Page::Incidence(incidence);
        }
        Page::Incidence(incidence) => {
            incidence.mutated = true;
            for i in 0..new.offsets.len() {
                incidence.add(new.offsets[i], new.mask(i, mw));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worker side
// ---------------------------------------------------------------------------

/// One blob's matches, encoded as pages, for [`Membership::install`].
#[derive(Default)]
pub(super) struct PageBatch(Vec<(u64, Page)>);

/// A worker's scratch for turning one blob's (ID, region) matches into a
/// [`PageBatch`]. Matches arrive in any order and may repeat.
pub(super) struct BatchBuilder {
    mw: usize,
    pairs: Vec<(i64, u32)>,
    entries: Entries,
}

impl BatchBuilder {
    pub(super) fn new(regions: usize) -> Self {
        Self {
            mw: mask_words(regions),
            pairs: Vec::new(),
            entries: Entries::default(),
        }
    }

    /// Record that `region` holds `id`. Negative IDs are dropped.
    #[inline]
    pub(super) fn push(&mut self, id: i64, region: usize) {
        if id >= 0 {
            self.pairs.push((id, region as u32));
        }
    }

    /// Record every region of `mask` for `id`.
    pub(super) fn push_mask(&mut self, id: i64, mask: &[u64]) {
        for region in regions_of(mask) {
            self.push(id, region);
        }
    }

    /// Encode everything pushed since the last call and clear.
    pub(super) fn finish(&mut self) -> PageBatch {
        let mw = self.mw;
        self.pairs.sort_unstable();
        let mut batch = Vec::new();
        let mut current: Option<u64> = None;
        self.entries.clear();
        for &(id, region) in &self.pairs {
            let Some((key, off)) = split(id) else {
                continue;
            };
            if current != Some(key) {
                if let Some(done) = current
                    && !self.entries.offsets.is_empty()
                {
                    batch.push((done, encode(&self.entries, mw, 0)));
                }
                self.entries.clear();
                current = Some(key);
            }
            if self.entries.offsets.last() != Some(&off) {
                self.entries.offsets.push(off);
                self.entries.masks.extend(std::iter::repeat_n(0, mw));
            }
            let at = self.entries.masks.len() - mw;
            self.entries.masks[at + region as usize / 64] |= 1u64 << (region % 64);
        }
        if let Some(done) = current
            && !self.entries.offsets.is_empty()
        {
            batch.push((done, encode(&self.entries, mw, 0)));
        }
        self.pairs.clear();
        PageBatch(batch)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Feed `blobs` of (id, region) matches in the given order and compare
    /// every probed ID's mask with per-region reference sets.
    fn check(regions: usize, blobs: &[Vec<(i64, usize)>], probes: &[i64], freeze: bool) {
        let mut reference: Vec<BTreeSet<i64>> = vec![BTreeSet::new(); regions];
        let mut membership = Membership::new(regions);
        let mut builder = BatchBuilder::new(regions);
        for blob in blobs {
            for &(id, region) in blob {
                builder.push(id, region);
                if id >= 0 {
                    reference[region].insert(id);
                }
            }
            membership.install(builder.finish());
        }
        if freeze {
            membership.freeze();
        }
        let mut mask = vec![0u64; mask_words(regions)];
        let all: BTreeSet<i64> = reference.iter().flatten().copied().collect();
        for &id in probes.iter().chain(all.iter()) {
            let member = membership.mask_into(id, &mut mask);
            let got: Vec<usize> = regions_of(&mask).collect();
            let want: Vec<usize> = (0..regions)
                .filter(|&r| reference[r].contains(&id))
                .collect();
            assert_eq!(got, want, "id {id}");
            assert_eq!(member, !want.is_empty(), "id {id}");
        }
    }

    fn random_blobs(
        rng: &mut Rng,
        regions: usize,
        blobs: usize,
        per: usize,
        span: u64,
    ) -> Vec<Vec<(i64, usize)>> {
        (0..blobs)
            .map(|_| {
                (0..per)
                    .map(|_| (rng.below(span) as i64, rng.below(regions as u64) as usize))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn differential_random_order_and_overlap() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for &regions in &[1usize, 2, 5, 17, 64, 65, 130] {
            for &span in &[3_000u64, 40_000, 1 << 26] {
                let blobs = random_blobs(&mut rng, regions, 40, 600, span);
                let probes: Vec<i64> = (0..2_000).map(|_| rng.below(span + 10) as i64).collect();
                check(regions, &blobs, &probes, false);
                check(regions, &blobs, &probes, true);
            }
        }
    }

    #[test]
    fn honest_sorted_blobs_with_shared_boundary_pages() {
        let mut blobs = Vec::new();
        let mut id = 0i64;
        for b in 0..50 {
            let mut blob = Vec::new();
            for _ in 0..1_000 {
                id += 1 + (id % 3);
                blob.push((id, (id as usize + b) % 5));
            }
            blobs.push(blob);
        }
        check(5, &blobs, &[0, 1, 2, 4095, 4096, 4097], true);
    }

    #[test]
    fn repeated_revisits_convert_to_incidence_and_stay_exact() {
        // A full page, then the same IDs again in other regions, many times.
        let base: Vec<i64> = (0..4096).collect();
        let mut blobs = vec![base.iter().map(|&id| (id, 0)).collect::<Vec<_>>()];
        for round in 1..8 {
            blobs.push(
                base.iter()
                    .step_by(round)
                    .map(|&id| (id, round % 6))
                    .collect(),
            );
        }
        blobs.push(vec![(7, 3)]);
        check(6, &blobs, &[4096, 9999], false);
        check(6, &blobs, &[4096, 9999], true);
    }

    #[test]
    fn isolated_and_extreme_ids() {
        let blobs = vec![
            vec![(i64::MAX, 0), (i64::MAX - 1, 1), (1 << 43, 0)],
            vec![(-1, 0), (i64::MIN, 1), (0, 1)],
            (0..2_000)
                .map(|i| (i * (1 << 25), i as usize % 3))
                .collect(),
        ];
        check(
            3,
            &blobs,
            &[-1, i64::MIN, 1, (1 << 25) + 1, i64::MAX - 2],
            true,
        );
    }

    #[test]
    fn many_distinct_masks_on_one_page() {
        // 4096 members, each with its own mask over 13 regions.
        let blobs: Vec<Vec<(i64, usize)>> = vec![
            (0..4096i64)
                .flat_map(|id| {
                    (0..13)
                        .filter(move |r| (id >> r) & 1 == 1)
                        .map(move |r| (id, r))
                })
                .collect(),
        ];
        check(13, &blobs, &[0], true);
    }

    #[test]
    fn full_mask_clears_unused_bits() {
        assert_eq!(full_mask(5), vec![0b1_1111]);
        assert_eq!(full_mask(64), vec![!0]);
        assert_eq!(full_mask(65), vec![!0, 1]);
    }
}
