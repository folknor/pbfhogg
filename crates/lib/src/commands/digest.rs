//! Digest of a PBF: `pbfhogg digest`, in one of two canonical forms.
//!
//! - **[`Form::Content`] (`v1`).** Two PBFs that the digest accepts get the
//!   same content digest exactly when `pbfhogg diff --suppress-common`
//!   reports them identical, barring a 128-bit hash collision. It is
//!   independent of encoding: compression, blob boundaries, dense versus
//!   plain nodes, string-table order, granularity, header fields, metadata
//!   and indexdata presence all leave it unchanged. What it covers is
//!   defined in one place, `crate::content`, which is also what every
//!   `diff` path compares with.
//! - **[`Form::ContentLocations`] (`v4`).** Content plus each way's inline
//!   `LocationsOnWays` coordinates normalized to decimicrodegrees: two PBFs
//!   that this form accepts get the same digest exactly when
//!   `pbfhogg diff --locations --suppress-common` reports them identical,
//!   barring a hash collision. Both forms are one relation each of
//!   `crate::content::Relation`. It refuses a file whose header does not
//!   declare `LocationsOnWays`, as `diff --locations` does.
//! - **[`Form::Full`] (`v3`).** Content plus everything a writer states
//!   about it: metadata with its presence, `visible`, exact coordinates,
//!   `LocationsOnWays` way coordinates, the declared prepass fields and the
//!   header fields (folded into the first line). Two accepted files get the
//!   same full digest exactly when they have the same header and the same
//!   elements field for field, as `crate::content` "The full form"
//!   defines; still independent of compression, blob boundaries,
//!   string-table order, granularity and indexdata. It is no `diff`
//!   relation: it is for checking that two writers of one operation agree.
//!
//! A version number names one canonical form and is never reused: a change
//! to either form's definition or serialization takes the next unused
//! number, so a stored report is only compared with one of the same version.
//!
//! # The domain
//!
//! `diff`'s answer is a function of element content only on inputs where its
//! optimised paths are sound, so the digest refuses everything else: it
//! returns an error and prints nothing. Refusal is the safe direction for a
//! cache built on the digest - it falls back to running the real
//! comparison. Both forms accept the same inputs, except that the full form
//! also refuses malformed declared prepass fields (rule 5). Accepted inputs
//! satisfy all of:
//!
//! 1. **Envelope.** The first blob is an `OsmHeader` that decodes and claims
//!    `Sort.Type_then_ID` (diff refuses unsorted input). No later blob is an
//!    `OsmHeader` and no blob has an unknown type. A header-only file is valid
//!    and has the empty digest.
//! 2. **Order and shape.** Every data block's elements are of exactly one
//!    kind, or it has none ([`PrimitiveBlock::element_kinds`], the kinds its
//!    elements actually are, not the first-field label `block_type`), so
//!    diff's streaming merge routes it correctly. Groups that yield no
//!    element (an empty dense message, changeset or unknown fields) do not
//!    count. Kinds appear
//!    nodes, then ways, then relations. Within a kind, IDs strictly ascend in
//!    canonical OSM order across the whole file: no duplicates, no disorder.
//! 3. **Indexdata.** Either every data blob carries readable indexdata or
//!    none does (presence as diff's own readers see it: an unreadable index
//!    counts as absent). Where present, every claim diff relies on is exact
//!    against the decoded block: kind, numeric minimum and maximum ID, and
//!    element count. An empty block carrying indexdata is refused. Diff's
//!    block-pair engines trust these claims, so a file whose claims lie is
//!    one on which diff can disagree with its own content.
//! 4. **References.** Every string-table reference the decoder exposes
//!    resolves: tag keys and values, member roles, and metadata user names.
//! 5. **Declared prepass fields (full form only).** Where the header
//!    declares `pbfhogg.WayMembers-v1`, every way blob carries a well-formed
//!    field-5 payload whose way count is the block's and whose padding bits
//!    are clear. Where it declares `pbfhogg.SharedNodePins-v1`, every pin
//!    bitmap a way carries is one bit per ref, rounded up to whole bytes,
//!    with no bit past the last ref. Undeclared payloads are not read.
//!
//! The promise is "refuses input that fails the validation above", not
//! "refuses every corrupt file". In particular a file truncated exactly at a
//! blob boundary is a valid shorter file: the format has no end marker.
//!
//! # Output
//!
//! [`Digest::write_report`] prints one line naming the algorithm and the
//! form's version, then one line per element kind with its count and the
//! digest of that kind alone:
//!
//! ```text
//! xxh128 v<N> <32 hex digits>
//! node <count> <32 hex digits>
//! way <count> <32 hex digits>
//! relation <count> <32 hex digits>
//! ```
//!
//! Each kind digest is XXH3-128 over the form's record stream
//! (`crate::content::write_record` or
//! `crate::content::write_full_record`) of that kind's elements, in file
//! order. The first line's digest is, for the content form, XXH3-128 over
//! every element's record in file order; for the full form, XXH3-128 over
//! [`FULL_DOMAIN`], the header record
//! (`crate::content::write_header_record`), then for node, way and
//! relation in that order the kind byte, the count as u64 LE and the kind
//! digest as u128 LE. So in the full form a header difference moves the
//! first line alone.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use xxhash_rust::xxh3::Xxh3;

use crate::blob::{Blob, BlobType};
use crate::blob_meta::BlobIndex;
use crate::content::{Prepass, Relation, write_full_record, write_header_record, write_record};
use crate::error::{Error, Location};
use crate::read::block::GroupProjection;
use crate::read::input::InputSource;
use crate::read::pipeline::{DecodeTransform, Decoder};
use crate::read::wire::{PackedSint32Iter, WireDenseInfo, WireDenseNodes};
use crate::tunables::DIGEST_RECORD_BUFFERS_KEPT;
use crate::{ByteOffset, Element, ElementKind, ElementReader, PrimitiveBlock};

use super::Result;

/// The hash algorithm, as printed on the first report line.
pub const ALGORITHM: &str = "xxh128";

/// The bytes the full form's first-line hash starts with, so it can never
/// be mistaken for a hash over a record stream.
pub const FULL_DOMAIN: &[u8] = b"pbfhogg-digest-v3\0";

/// A canonical form of the digest: what it covers and how it is
/// serialized. Each form has its own version number, printed on the first
/// report line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Form {
    /// Element content, the equality `diff --suppress-common` reports
    /// (`v1`).
    Content,
    /// Element content plus inline way coordinates at decimicrodegrees, the
    /// equality `diff --locations --suppress-common` reports (`v4`).
    ContentLocations,
    /// Content, metadata, way coordinates, declared prepass fields and the
    /// header (`v3`; `v2` read a negative-version dense slot as no
    /// metadata, a dense changeset of -1 as a claim of -1 (now 0), and refused
    /// partial-column `DenseInfo`).
    Full,
}

impl Form {
    /// The version the report names. A number is never reused: changing a
    /// form's definition or serialization gives it the next unused one.
    #[must_use]
    pub const fn version(self) -> u32 {
        match self {
            Form::Content => 1,
            Form::Full => 3,
            Form::ContentLocations => 4,
        }
    }

    /// The equality relation a record form hashes; `None` for the full
    /// form, which is no diff relation.
    const fn relation(self) -> Option<Relation> {
        match self {
            Form::Content => Some(Relation::Content),
            Form::ContentLocations => Some(Relation::ContentLocations),
            Form::Full => None,
        }
    }

    /// The form's name, as `digest --form` spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Form::Content => "content",
            Form::ContentLocations => "content-locations",
            Form::Full => "full",
        }
    }
}

/// Element count and digest for one element kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindDigest {
    pub count: u64,
    pub hash: u128,
}

/// The digest of a PBF in one form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Digest {
    /// The form it was computed in, whose version the report prints.
    pub form: Form,
    /// The first report line's digest: every element (content form), or the
    /// header and the three kind digests (full form).
    pub all: u128,
    pub nodes: KindDigest,
    pub ways: KindDigest,
    pub relations: KindDigest,
}

impl Digest {
    /// Write the report: the algorithm line, then one line per kind.
    pub fn write_report(&self, w: &mut impl Write) -> std::io::Result<()> {
        writeln!(w, "{ALGORITHM} v{} {:032x}", self.form.version(), self.all)?;
        for (name, kind) in [
            ("node", self.nodes),
            ("way", self.ways),
            ("relation", self.relations),
        ] {
            writeln!(w, "{name} {} {:032x}", kind.count, kind.hash)?;
        }
        Ok(())
    }
}

/// Compute the digest of `input` in `form`, or refuse it.
///
/// Returns an error, and no digest, for unreadable input and for any input
/// outside the domain described in the module documentation.
///
/// The input is read in one sequential pass through one open, which checks
/// the envelope and the indexdata claims as it decodes, so a stream (a FIFO,
/// `/dev/stdin`) is digested like a file, and a file renamed over the input
/// mid-run cannot mix two files into one digest. A caller's
/// [`PinnedInput`](crate::read::input::PinnedInput) is opened through as it
/// is; none is taken here.
#[hotpath::measure]
pub fn digest<'a>(
    input: impl Into<InputSource<'a>>,
    form: Form,
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<Digest> {
    let host = &host.enter()?;
    let input = input.into();
    host.invocation
        .decided(|| digest_inner(input, form, direct_io, host))
}

fn digest_inner(
    input: InputSource<'_>,
    form: Form,
    direct_io: bool,
    host: &crate::host::Entered<'_>,
) -> Result<Digest> {
    let path = input.path();
    // The one open of the input. The leading-header rule (domain rule 1) is
    // the reader's: it refuses a file whose first blob is not an OsmHeader,
    // and any later OsmHeader blob.
    let reader = ElementReader::open_in(input, direct_io, host)?;
    let header = reader.header();
    if !header.is_sorted() {
        return Err(crate::error::new_unsorted(
            "the header does not claim Sort.Type_then_ID; digest covers sorted input only, \
             as diff does.",
        )
        .locate(Location::in_file(path)));
    }
    if form == Form::ContentLocations && !header.has_locations_on_ways() {
        return Err(Error::invalid_argument(
            "the content-locations digest covers inline way coordinates, but the header does \
             not declare LocationsOnWays",
        )
        .locate(Location::in_file(path)));
    }
    let full = match form {
        Form::Content | Form::ContentLocations => None,
        Form::Full => {
            let mut header_record = Vec::new();
            write_header_record(header, &mut header_record)
                .map_err(|e| e.locate(Location::in_file(path)))?;
            Some(Full {
                header_record,
                prepass: Prepass {
                    pins: header.has_shared_node_pins_v1(),
                },
                way_members: header.has_way_members_v1(),
            })
        }
    };
    let buffers = Arc::new(RecordBuffers::default());
    let transform = DigestTransform {
        relation: form.relation().unwrap_or(Relation::Content),
        full: full.as_ref().map(|f| (f.prepass, f.way_members)),
        buffers: Arc::clone(&buffers),
    };
    // Field 5 is read only where it is authoritative and asked for.
    let reader = reader.parsing_way_members(full.as_ref().is_some_and(|f| f.way_members));

    host.invocation.marker("DIGEST_SCAN_START");
    let mut state = DigestState::new(Arc::from(path), full.is_none(), buffers);
    reader.for_each_fused_blob(transform, |summary| state.absorb(summary))?;
    let digest = state.finish(form, full.as_ref().map(|f| f.header_record.as_slice()));
    host.invocation.marker("DIGEST_SCAN_END");
    Ok(digest)
}

/// What the full form knows from the header before the scan.
struct Full {
    header_record: Vec<u8>,
    prepass: Prepass,
    /// `pbfhogg.WayMembers-v1` is declared.
    way_members: bool,
}

/// The decode-worker side of the pass: refuses a blob of unknown type
/// (domain rule 1) and summarizes each data blob with its indexdata and
/// provenance, which the ordered consumer checks.
struct DigestTransform {
    /// The relation a record form hashes; unused by the full form.
    relation: Relation,
    /// `None` for the record forms; the full form's prepass declarations
    /// and whether `pbfhogg.WayMembers-v1` is declared.
    full: Option<(Prepass, bool)>,
    buffers: Arc<RecordBuffers>,
}

/// Record buffers the ordered consumer has hashed, for the decode workers
/// to serialize later blocks into.
///
/// A planet blob holds about 200k elements, so one block's full-form
/// records run to about 14 MB. Built fresh per block, every worker grew such
/// a buffer from empty and freed it on the consumer: mapped, faulted and
/// unmapped per block on every worker at once. At planet that left the
/// workers off-CPU about a quarter of their time and the full form at
/// 106.8 s against 61.7 s with reuse (bygg, reference/performance/digest.md,
/// "digest v2 cost"). Buffers are cleared before reuse and pass by
/// ownership, so no bytes cross between blocks; a buffer dropped on an
/// error path is simply not reused.
#[derive(Default)]
struct RecordBuffers(std::sync::Mutex<Vec<Vec<u8>>>);

impl RecordBuffers {
    fn take(&self) -> Vec<u8> {
        let reused = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop();
        reused.unwrap_or_default()
    }

    fn give(&self, mut buffer: Vec<u8>) {
        buffer.clear();
        let mut kept = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if kept.len() < DIGEST_RECORD_BUFFERS_KEPT {
            kept.push(buffer);
        }
    }
}

impl DecodeTransform for DigestTransform {
    type Out = BlockSummary;
    const PARSE_INDEXDATA: bool = true;

    fn apply(&self, blob: &Blob, decode: Decoder<'_>) -> Option<crate::Result<BlockSummary>> {
        match blob.get_type() {
            BlobType::OsmData => {}
            // The reader refuses a misplaced header before it gets here;
            // refused again rather than skipped, so the domain does not
            // hang on that.
            BlobType::OsmHeader => {
                return Some(Err(Error::invalid_data(
                    "a second OsmHeader blob; digest refuses it",
                )));
            }
            BlobType::Unknown(name) => {
                return Some(Err(Error::invalid_data(format!(
                    "blob of unknown type {name:?}; digest refuses it"
                ))));
            }
        }
        let records = match self.full {
            None => Records::Content(self.relation),
            Some((prepass, declared)) => Records::Full {
                prepass,
                way_members: declared.then(|| blob.way_members_checked()),
            },
        };
        Some(self.summarize(decode, records).map(|content| BlockSummary {
            index: blob.index(),
            blob: blob.blob_index(),
            offset: blob.offset(),
            content,
        }))
    }
}

impl DigestTransform {
    fn summarize(
        &self,
        decode: Decoder<'_>,
        records: Records<'_>,
    ) -> crate::Result<Option<BlockContent>> {
        if matches!(records, Records::Content(_)) {
            return decode.buffer(|buffer| {
                let (users, block) = buffer.project(|_, _| DenseUsers::default())?;
                summarize_block(&block.into_block(), records, &self.buffers, Some(&users))
            });
        }
        summarize_block(&decode.block()?, records, &self.buffers, None)
    }
}

/// Keep the packed user column and the group boundaries from block validation.
/// Copy only this column so building the block can move or grow its byte buffer.
#[derive(Default)]
struct DenseUsers {
    groups: Vec<(usize, usize)>,
    packed: Vec<u8>,
}

impl GroupProjection for DenseUsers {
    fn dense(&mut self, _: &WireDenseNodes<'_>, ids: usize, info: Option<&WireDenseInfo<'_>>) {
        if ids != 0 {
            if let Some(info) = info {
                self.packed.extend_from_slice(info.user_sid_data);
            }
            self.groups.push((ids, self.packed.len()));
        }
    }
}

struct DenseUserCursor<'a> {
    groups: std::slice::Iter<'a, (usize, usize)>,
    packed: &'a [u8],
    end: usize,
    remaining: usize,
    deltas: Option<PackedSint32Iter<'a>>,
    sid: i32,
}

impl<'a> DenseUserCursor<'a> {
    fn new(users: &'a DenseUsers) -> Self {
        Self {
            groups: users.groups.iter(),
            packed: &users.packed,
            end: 0,
            remaining: 0,
            deltas: None,
            sid: 0,
        }
    }

    fn start_group(&mut self) -> crate::Result<()> {
        let Some(&(count, end)) = self.groups.next() else {
            return Err(Error::internal(
                "digest dense user cursor ran out of groups",
            ));
        };
        let packed = self
            .packed
            .get(self.end..end)
            .ok_or_else(|| Error::internal("digest dense user column has an invalid range"))?;
        self.end = end;
        self.remaining = count;
        self.deltas = (!packed.is_empty()).then(|| PackedSint32Iter::new(packed));
        self.sid = 0;
        Ok(())
    }

    fn check_next(&mut self, table_len: usize) -> crate::Result<()> {
        if self.remaining == 0 {
            self.start_group()?;
        }
        self.remaining -= 1;
        let Some(deltas) = &mut self.deltas else {
            return Ok(());
        };
        let delta = deltas
            .next()
            .ok_or_else(|| Error::internal("digest dense user column ended early"))?;
        if self.remaining == 0 && deltas.next().is_some() {
            return Err(Error::internal("digest dense user column has extra deltas"));
        }
        self.sid = self.sid.wrapping_add(delta);
        let index = usize::try_from(self.sid).map_err(|_| {
            crate::error::new_wire_error("dense node info has negative user string index")
        })?;
        // Block validation already checked UTF-8 for every table entry.
        if index >= table_len {
            return Err(crate::error::new_stringtable_index_out_of_bounds(index));
        }
        Ok(())
    }

    fn finish(&self) -> crate::Result<()> {
        if self.remaining != 0 || !self.groups.as_slice().is_empty() {
            return Err(Error::internal(
                "digest dense user cursor has unvisited nodes",
            ));
        }
        Ok(())
    }
}

/// A blob's field-5 payload as (encoded way count, bitmap).
type WayMembers<'b> = (u32, &'b [u8]);

/// Which records a block's elements are serialized as.
enum Records<'b> {
    /// A record form, under its relation.
    Content(Relation),
    Full {
        prepass: Prepass,
        /// `Some` where the header declares `pbfhogg.WayMembers-v1`: the
        /// blob's checked field-5 payload, `None` inside when the blob
        /// carries none. Judged only for a way block.
        way_members: Option<crate::Result<Option<WayMembers<'b>>>>,
    },
}

/// The way-membership bit of the way at `ordinal` in its blob (domain rule
/// 5). Bits are LSB-first in block order, as the producer writes them.
fn way_member_bit(payload: Option<(u32, &[u8])>, ordinal: u64) -> crate::Result<bool> {
    let Some((count, bitmap)) = payload else {
        return Err(Error::invalid_data(
            "the header declares pbfhogg.WayMembers-v1 but this way blob carries no field-5 \
             payload",
        ));
    };
    if ordinal >= u64::from(count) {
        return Err(Error::invalid_data(format!(
            "the field-5 payload counts {count} way(s) but the block holds more"
        )));
    }
    let byte = usize::try_from(ordinal / 8)
        .ok() // ok-deliberate: an index past usize is past the bitmap
        .and_then(|i| bitmap.get(i))
        .copied()
        .unwrap_or(0);
    Ok(byte & (1 << (ordinal % 8)) != 0)
}

/// After a way block: the payload counted exactly the block's ways and set
/// no padding bit (domain rule 5).
fn check_way_member_tail(payload: Option<(u32, &[u8])>, ways: u64) -> crate::Result<()> {
    let Some((count, bitmap)) = payload else {
        return Ok(());
    };
    if u64::from(count) != ways {
        return Err(Error::invalid_data(format!(
            "the field-5 payload counts {count} way(s) but the block holds {ways}"
        )));
    }
    let used = count % 8;
    if used != 0
        && let Some(&last) = bitmap.last()
        && last >> used != 0
    {
        return Err(Error::invalid_data(
            "the field-5 payload sets a bit past its last way",
        ));
    }
    Ok(())
}

/// What one data blob contributes, computed on a decode worker.
struct BlockSummary {
    /// The blob's indexdata, `None` where absent or unreadable (presence as
    /// diff's own readers see it).
    index: Option<BlobIndex>,
    /// The blob's ordinal and frame start, as far as the reader knew them,
    /// to locate a refusal the consumer raises.
    blob: Option<u64>,
    offset: Option<ByteOffset>,
    /// `None` for a block with no elements.
    content: Option<BlockContent>,
}

struct BlockContent {
    kind: ElementKind,
    /// First and last element ID in file (and therefore canonical) order.
    first_id: i64,
    last_id: i64,
    /// Numeric extrema, for checking indexdata claims.
    min_id: i64,
    max_id: i64,
    count: u64,
    /// The block's canonical records, concatenated.
    records: Vec<u8>,
}

fn element_kind(element: &Element<'_>) -> ElementKind {
    match element {
        Element::DenseNode(_) | Element::Node(_) => ElementKind::Node,
        Element::Way(_) => ElementKind::Way,
        Element::Relation(_) => ElementKind::Relation,
    }
}

fn block_kind(block: &PrimitiveBlock) -> crate::Result<Option<ElementKind>> {
    let kinds = block.element_kinds();
    if kinds.is_empty() {
        return Ok(None);
    }
    kinds.single().map(Some).ok_or_else(|| {
        Error::invalid_data(
            "a data block holds elements of more than one kind; diff's \
             streaming merge rejects such blocks",
        )
    })
}

/// Validate one block internally (domain rules 2 and 4) and serialize it.
/// Runs on the decode workers.
fn summarize_block(
    block: &PrimitiveBlock,
    records: Records<'_>,
    buffers: &RecordBuffers,
    users: Option<&DenseUsers>,
) -> crate::Result<Option<BlockContent>> {
    let classified = block_kind(block)?;
    // The full form's per-block inputs. A way block in a file declaring
    // pbfhogg.WayMembers-v1 needs its field-5 payload (domain rule 5).
    let (relation, full) = match records {
        Records::Content(relation) => (relation, None),
        Records::Full {
            prepass,
            way_members,
        } => {
            let payload = match way_members {
                Some(checked) if classified == Some(ElementKind::Way) => Some(checked?),
                _ => None,
            };
            (Relation::Content, Some((prepass, payload)))
        }
    };
    let mut users = users.map(DenseUserCursor::new);
    let table_len = block.string_table_len();
    let elements = if users.is_some() {
        block.elements_skip_metadata()
    } else {
        block.elements()
    };
    let mut content: Option<BlockContent> = None;
    for element in elements {
        let kind = element_kind(&element);
        if classified != Some(kind) {
            return Err(Error::invalid_data(format!(
                "a data block classified as {} holds a {} element; diff's streaming merge \
                 would misroute it",
                classified.map_or("empty", ElementKind::as_str),
                kind.as_str(),
            )));
        }
        let id = crate::osc::merge_join::element_id(&element);
        check_record_user(&element, users.as_mut(), table_len)
            .map_err(|e| e.context(format!("{} {id}", kind.as_str())))?;
        let c = content.get_or_insert_with(|| BlockContent {
            kind,
            first_id: id,
            last_id: id,
            min_id: id,
            max_id: id,
            count: 0,
            // Taken at the first element, so an empty block takes none.
            records: buffers.take(),
        });
        if c.count > 0 && crate::osm_id::osm_id_cmp(c.last_id, id).is_ge() {
            return Err(Error::invalid_data(format!(
                "{} {id} follows {} {} in the same block; IDs must strictly ascend",
                kind.as_str(),
                kind.as_str(),
                c.last_id,
            )));
        }
        let written = match full {
            // The way already carries its parsed message, and write_record
            // streams its refs once. A separate way writer would duplicate
            // record encoding without removing a message or ref re-parse.
            None => write_record(&element, id, relation, &mut c.records),
            Some((prepass, payload)) => {
                let member = match payload {
                    Some(p) if kind == ElementKind::Way => way_member_bit(p, c.count).map(Some),
                    _ => Ok(None),
                };
                member.and_then(|m| write_full_record(&element, id, prepass, m, &mut c.records))
            }
        };
        written.map_err(|e| e.context(format!("{} {id}", kind.as_str())))?;
        c.last_id = id;
        c.min_id = c.min_id.min(id);
        c.max_id = c.max_id.max(id);
        c.count += 1;
    }
    if let Some(users) = users {
        users.finish()?;
    }
    if let Some((_, Some(payload))) = full {
        check_way_member_tail(payload, content.as_ref().map_or(0, |c| c.count))?;
    }
    Ok(content)
}

fn check_record_user(
    element: &Element<'_>,
    users: Option<&mut DenseUserCursor<'_>>,
    table_len: usize,
) -> crate::Result<()> {
    match (element, users) {
        (Element::DenseNode(_), Some(users)) => users.check_next(table_len),
        _ => check_user(element),
    }
}

/// Resolve the element's metadata user name, if it has one (domain rule 4).
fn check_user(element: &Element<'_>) -> crate::error::Result<()> {
    match element {
        Element::DenseNode(n) => {
            if let Some(info) = n.info() {
                info.user()?;
            }
        }
        Element::Node(n) => {
            n.info().user().transpose()?;
        }
        Element::Way(w) => {
            w.info().user().transpose()?;
        }
        Element::Relation(r) => {
            r.info().user().transpose()?;
        }
    }
    Ok(())
}

/// The in-order consumer: cross-block checks (domain rules 2 and 3) and the
/// streaming hash states. Nothing leaves it until [`Self::finish`], so a
/// refusal anywhere in the file prints no digest at all.
struct DigestState {
    /// The input, to locate the refusals raised here. Domain rule 3's
    /// either-or (a partially indexed file is refused, a wholly unindexed one
    /// digested) is not judged here: the reader refuses a partially indexed
    /// command input itself, located at its first unindexed data blob, as
    /// every command does (`Frames::refuse_partial_index`).
    path: Arc<Path>,
    /// Kind and ID of the last element seen.
    last: Option<(ElementKind, i64)>,
    /// The content form's first-line hash over every record; `None` for the
    /// full form, whose first line is computed from the kind digests.
    all: Option<Xxh3>,
    /// Content forms only: the node digest, taken from `all` when the first
    /// non-node record arrives. Nodes come first, so until then `all` has
    /// absorbed exactly the node record stream and its non-consuming
    /// `digest128` is the node line; node records are therefore hashed once,
    /// into `all`, and never into `kinds[0]`. `None` until taken; `finish`
    /// takes it from the final `all` when no way or relation arrived.
    node_hash: Option<u128>,
    kinds: [(u64, Xxh3); 3],
    /// Where a hashed block's record buffer goes back to.
    buffers: Arc<RecordBuffers>,
}

impl DigestState {
    fn new(path: Arc<Path>, hash_all_records: bool, buffers: Arc<RecordBuffers>) -> Self {
        Self {
            path,
            buffers,
            last: None,
            all: hash_all_records.then(Xxh3::new),
            node_hash: None,
            kinds: [(0, Xxh3::new()), (0, Xxh3::new()), (0, Xxh3::new())],
        }
    }

    /// Absorb one data blob's summary, in file order. A refusal is located
    /// at the blob that raised it.
    fn absorb(&mut self, summary: BlockSummary) -> crate::Result<()> {
        let (blob, offset) = (summary.blob, summary.offset);
        self.absorb_checked(summary)
            .map_err(|e| e.locate_blob(Location::new(Some(&self.path), blob, offset)))
    }

    fn absorb_checked(&mut self, summary: BlockSummary) -> crate::Result<()> {
        // Each summary carries its own blob's index, so no alignment
        // between two readers of the file is needed.
        Self::check_index(summary.index, summary.content.as_ref())?;
        let Some(content) = summary.content else {
            return Ok(());
        };
        if let Some((last_kind, last_id)) = self.last {
            let (this, prev) = (content.kind, last_kind);
            if this < prev {
                return Err(Error::invalid_data(format!(
                    "a {} block follows {} elements; kinds must appear nodes, ways, relations",
                    content.kind.as_str(),
                    last_kind.as_str(),
                )));
            }
            if this == prev && crate::osm_id::osm_id_cmp(last_id, content.first_id).is_ge() {
                return Err(Error::invalid_data(format!(
                    "{} {} follows {} {} in an earlier block; IDs must strictly ascend",
                    content.kind.as_str(),
                    content.first_id,
                    last_kind.as_str(),
                    last_id,
                )));
            }
        }
        self.last = Some((content.kind, content.last_id));
        let (count, hasher) = &mut self.kinds[usize::from(content.kind.ordinal())];
        *count += content.count;
        match &mut self.all {
            Some(all) if content.kind == ElementKind::Node => all.update(&content.records),
            Some(all) => {
                if self.node_hash.is_none() {
                    self.node_hash = Some(all.digest128());
                }
                all.update(&content.records);
                hasher.update(&content.records);
            }
            None => hasher.update(&content.records),
        }
        // Hashing stays here, serial. An exact split of these XXH3-128
        // streams - each 1024-byte block's lane sums computed on a pool,
        // folded here in order, bit-identical output, no new version - was
        // built and measured at planet and did not beat this (66.2 s
        // against 61.7 s, bygg, reference/performance/digest.md, "digest v2
        // cost"). Reconsider it only if serialization gets faster or this
        // hashing is shown to be the limit.
        self.buffers.give(content.records);
        Ok(())
    }

    /// Check one blob's indexdata against its decoded block. Whether the
    /// file is indexed throughout is the partial-index guard's question.
    fn check_index(index: Option<BlobIndex>, content: Option<&BlockContent>) -> crate::Result<()> {
        let Some(index) = index else {
            return Ok(());
        };
        let Some(c) = content else {
            return Err(Error::invalid_data("an empty data block carries indexdata"));
        };
        if index.kind != c.kind
            || index.min_id != c.min_id
            || index.max_id != c.max_id
            || index.count != c.count
        {
            return Err(Error::invalid_data(format!(
                "a {} blob's indexdata claims kind {}, ids {}..={}, count {}, but the block \
                 holds ids {}..={}, count {}; diff trusts these claims",
                c.kind.as_str(),
                index.kind.as_str(),
                index.min_id,
                index.max_id,
                index.count,
                c.min_id,
                c.max_id,
                c.count,
            )));
        }
        Ok(())
    }

    /// The digest. `header_record` is the full form's, `None` for the
    /// content form.
    fn finish(self, form: Form, header_record: Option<&[u8]>) -> Digest {
        let [mut nodes, ways, relations] = self.kinds.map(|(count, hasher)| KindDigest {
            count,
            hash: hasher.digest128(),
        });
        if let Some(all) = &self.all {
            nodes.hash = self.node_hash.unwrap_or_else(|| all.digest128());
        }
        let all = match (self.all, header_record) {
            (Some(all), _) => all.digest128(),
            (None, header) => {
                let mut all = Xxh3::new();
                all.update(FULL_DOMAIN);
                all.update(header.unwrap_or_default());
                for (kind, d) in [
                    (ElementKind::Node, nodes),
                    (ElementKind::Way, ways),
                    (ElementKind::Relation, relations),
                ] {
                    all.update(&[kind_byte(kind)]);
                    all.update(&d.count.to_le_bytes());
                    all.update(&d.hash.to_le_bytes());
                }
                all.digest128()
            }
        };
        Digest {
            form,
            all,
            nodes,
            ways,
            relations,
        }
    }
}

/// The kind byte the record streams use, for the full form's first line.
fn kind_byte(kind: ElementKind) -> u8 {
    match kind {
        ElementKind::Node => crate::content::KIND_NODE,
        ElementKind::Way => crate::content::KIND_WAY,
        ElementKind::Relation => crate::content::KIND_RELATION,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DenseUsers, Prepass, PrimitiveBlock, RecordBuffers, Records, Relation,
        check_way_member_tail, summarize_block, way_member_bit,
    };
    use crate::read::block::BlockBuffer;

    fn field(number: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![
            (number << 3) | 2,
            u8::try_from(body.len()).expect("fixture"),
        ];
        bytes.extend_from_slice(body);
        bytes
    }

    fn dense_group(ids: &[u8], lats: &[u8], users: &[u8]) -> Vec<u8> {
        let mut info = field(1, &vec![1; ids.len()]);
        info.extend(field(5, users));
        let mut dense = field(1, ids);
        dense.extend(field(5, &info));
        dense.extend(field(8, lats));
        dense.extend(field(9, &vec![0; ids.len()]));
        field(2, &field(2, &dense))
    }

    fn block_and_users(groups: &[Vec<u8>]) -> (PrimitiveBlock, DenseUsers) {
        let mut table = field(1, b"");
        table.extend(field(1, b"user"));
        let mut bytes = field(1, &table);
        for group in groups {
            bytes.extend_from_slice(group);
        }
        let mut strings = Vec::new();
        let mut offsets = Vec::new();
        let (users, block) = BlockBuffer::new(bytes, None, &mut strings, &mut offsets, None)
            .project(|_, _| DenseUsers::default())
            .expect("fixture");
        (block.into_block(), users)
    }

    #[test]
    fn content_dense_user_cursor_preserves_record_bytes_across_groups() {
        let (block, users) = block_and_users(&[
            dense_group(&[2, 2], &[0, 0], &[2, 1]), // IDs 1, 2; SIDs 1, 0
            field(2, &field(1, &[8, 6, 64, 0, 72, 0])), // Plain node ID 3
            dense_group(&[], &[], &[]),
            dense_group(&[8], &[0], &[]),   // ID 4, no user column
            dense_group(&[10], &[0], &[2]), // ID 5, SID 1
            dense_group(&[12], &[0], &[2]), // ID 6, SID restarts at 1
        ]);
        let buffers = RecordBuffers::default();
        for relation in [Relation::Content, Relation::ContentLocations] {
            let baseline = summarize_block(&block, Records::Content(relation), &buffers, None)
                .expect("fixture")
                .expect("fixture");
            let projected =
                summarize_block(&block, Records::Content(relation), &buffers, Some(&users))
                    .expect("fixture")
                    .expect("fixture");
            assert_eq!(projected.count, 6);
            assert_eq!(projected.records, baseline.records);
        }
    }

    #[test]
    fn dense_user_error_precedes_id_order_and_record_errors() {
        // Both IDs are 1. The second latitude is i64::MAX before scaling,
        // so serializing that node would also fail.
        let lats = [0, 0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1];
        let buffers = RecordBuffers::default();
        for (delta, message) in [
            (1, "negative user string index"),
            (4, "stringtable index out of bounds"),
            (2, "IDs must strictly ascend"),
        ] {
            let (block, users) = block_and_users(&[dense_group(&[2, 0], &lats, &[0, delta])]);
            for (records, users) in [
                (Records::Content(Relation::Content), None),
                (Records::Content(Relation::Content), Some(&users)),
                (Records::Content(Relation::ContentLocations), Some(&users)),
                (
                    Records::Full {
                        prepass: Prepass::default(),
                        way_members: None,
                    },
                    None,
                ),
            ] {
                let error = summarize_block(&block, records, &buffers, users)
                    .err()
                    .expect("second node must fail");
                assert!(error.to_string().contains(message), "{error}");
            }
        }
    }

    /// Bits are LSB-first in block order; the payload must count exactly
    /// the block's ways and leave its padding clear.
    #[test]
    fn way_member_bits_and_tail() {
        let bitmap = [0b1000_0001u8, 0b0000_0001];
        let payload = Some((9, &bitmap[..]));
        let bits: Vec<bool> = (0..9)
            .map(|i| way_member_bit(payload, i).unwrap_or_else(|e| panic!("{e}")))
            .collect();
        assert_eq!(
            bits,
            [true, false, false, false, false, false, false, true, true]
        );
        crate::test_support::assert_err_kind!(
            way_member_bit(payload, 9),
            crate::ErrorKind::InvalidData { msg, .. } if msg.contains("the block holds more"),
            "more ways than counted"
        );
        crate::test_support::assert_err_kind!(
            way_member_bit(None, 0),
            crate::ErrorKind::InvalidData { msg, .. } if msg.contains("carries no field-5"),
            "declared but absent",
        );
        assert!(check_way_member_tail(payload, 9).is_ok());
        crate::test_support::assert_err_kind!(
            check_way_member_tail(payload, 8),
            crate::ErrorKind::InvalidData { msg, .. } if msg.contains("the block holds 8"),
            "fewer ways",
        );
        let padded = [0u8, 0b0000_0010];
        crate::test_support::assert_err_kind!(
            check_way_member_tail(Some((9, &padded[..])), 9),
            crate::ErrorKind::InvalidData { msg, .. } if msg.contains("past its last way"),
            "a padding bit set"
        );
        assert!(check_way_member_tail(Some((8, &[0xFF][..])), 8).is_ok());
    }
}
