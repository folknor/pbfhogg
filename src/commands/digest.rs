//! Digest of a PBF: `pbfhogg digest`, in one of two canonical forms.
//!
//! - **[`Form::Content`] (`v1`).** Two PBFs that the digest accepts get the
//!   same content digest exactly when `pbfhogg diff --suppress-common`
//!   reports them identical, barring a 128-bit hash collision. It is
//!   independent of encoding: compression, blob boundaries, dense versus
//!   plain nodes, string-table order, granularity, header fields, metadata
//!   and indexdata presence all leave it unchanged. What it covers is
//!   defined in one place, [`crate::content`], which is also what every
//!   `diff` path compares with.
//! - **[`Form::Full`] (`v2`).** Content plus everything a writer states
//!   about it: metadata with its presence, `visible`, exact coordinates,
//!   `LocationsOnWays` way coordinates, the declared prepass fields and the
//!   header fields (folded into the first line). Two accepted files get the
//!   same full digest exactly when they have the same header and the same
//!   elements field for field, as [`crate::content`] "The full form"
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
//! 2. **Order and shape.** Every data block is classified by
//!    [`PrimitiveBlock::block_type`] as exactly one element kind, or as
//!    empty with no elements - never mixed - and every element in it is of
//!    that kind, so diff's streaming merge routes it correctly. Kinds appear
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
//! ([`crate::content::write_record`] or
//! [`crate::content::write_full_record`]) of that kind's elements, in file
//! order. The first line's digest is, for the content form, XXH3-128 over
//! every element's record in file order; for the full form, XXH3-128 over
//! [`FULL_DOMAIN`], the header record
//! ([`crate::content::write_header_record`]), then for node, way and
//! relation in that order the kind byte, the count as u64 LE and the kind
//! digest as u128 LE. So in the full form a header difference moves the
//! first line alone.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use xxhash_rust::xxh3::Xxh3;

use crate::blob::{Blob, BlobType};
use crate::blob_meta::BlobIndex;
use crate::content::{Prepass, write_full_record, write_header_record, write_record};
use crate::error::{Error, Location};
use crate::read::input::InputSource;
use crate::read::pipeline::DecodeTransform;
use crate::{BlockType, ByteOffset, Element, ElementKind, ElementReader, PrimitiveBlock};

use super::Result;

/// The hash algorithm, as printed on the first report line.
pub const ALGORITHM: &str = "xxh128";

/// The bytes the full form's first-line hash starts with, so it can never
/// be mistaken for a hash over a record stream.
pub const FULL_DOMAIN: &[u8] = b"pbfhogg-digest-v2\0";

/// A canonical form of the digest: what it covers and how it is
/// serialized. Each form has its own version number, printed on the first
/// report line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Form {
    /// Element content, the equality `diff --suppress-common` reports
    /// (`v1`).
    Content,
    /// Content, metadata, way coordinates, declared prepass fields and the
    /// header (`v2`).
    Full,
}

impl Form {
    /// The version the report names. A number is never reused: changing a
    /// form's definition or serialization gives it the next unused one.
    #[must_use]
    pub const fn version(self) -> u32 {
        match self {
            Form::Content => 1,
            Form::Full => 2,
        }
    }

    /// The form's name, as `digest --form` spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Form::Content => "content",
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
    let full = match form {
        Form::Content => None,
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
    let transform = DigestTransform {
        full: full.as_ref().map(|f| (f.prepass, f.way_members)),
    };
    // Field 5 is read only where it is authoritative and asked for.
    let reader = reader.parsing_way_members(full.as_ref().is_some_and(|f| f.way_members));

    host.invocation.marker("DIGEST_SCAN_START");
    let mut state = DigestState::new(Arc::from(path), full.is_none());
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
    /// `None` for the content form; the full form's prepass declarations
    /// and whether `pbfhogg.WayMembers-v1` is declared.
    full: Option<(Prepass, bool)>,
}

impl DecodeTransform for DigestTransform {
    type Out = BlockSummary;
    const PARSE_INDEXDATA: bool = true;

    fn apply<D>(&self, blob: &Blob, decode: D) -> Option<crate::Result<BlockSummary>>
    where
        D: FnOnce() -> crate::Result<PrimitiveBlock>,
    {
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
            None => Records::Content,
            Some((prepass, declared)) => Records::Full {
                prepass,
                way_members: declared.then(|| blob.way_members_checked()),
            },
        };
        Some(decode().and_then(|block| {
            Ok(BlockSummary {
                index: blob.index(),
                blob: blob.blob_index(),
                offset: blob.offset(),
                content: summarize_block(block, records)?,
            })
        }))
    }
}

/// A blob's field-5 payload as (encoded way count, bitmap).
type WayMembers<'b> = (u32, &'b [u8]);

/// Which records a block's elements are serialized as.
enum Records<'b> {
    Content,
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

fn block_kind(block_type: BlockType) -> crate::Result<Option<ElementKind>> {
    match block_type {
        BlockType::DenseNodes | BlockType::Nodes => Ok(Some(ElementKind::Node)),
        BlockType::Ways => Ok(Some(ElementKind::Way)),
        BlockType::Relations => Ok(Some(ElementKind::Relation)),
        BlockType::Empty => Ok(None),
        BlockType::Mixed => Err(Error::invalid_data(
            "a data block mixes element kinds (or carries an unknown \
             group field); diff's streaming merge rejects such blocks",
        )),
    }
}

/// Validate one block internally (domain rules 2 and 4) and serialize it.
/// Runs on the decode workers. Takes the block by value because the fused
/// pipeline hands ownership to its transform.
#[allow(clippy::needless_pass_by_value)]
fn summarize_block(
    block: PrimitiveBlock,
    records: Records<'_>,
) -> crate::Result<Option<BlockContent>> {
    let classified = block_kind(block.block_type())?;
    // The full form's per-block inputs. A way block in a file declaring
    // pbfhogg.WayMembers-v1 needs its field-5 payload (domain rule 5).
    let full = match records {
        Records::Content => None,
        Records::Full {
            prepass,
            way_members,
        } => {
            let payload = match way_members {
                Some(checked) if classified == Some(ElementKind::Way) => Some(checked?),
                _ => None,
            };
            Some((prepass, payload))
        }
    };
    let mut content: Option<BlockContent> = None;
    for element in block.elements() {
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
        check_user(&element).map_err(|e| e.context(format!("{} {id}", kind.as_str())))?;
        let c = content.get_or_insert_with(|| BlockContent {
            kind,
            first_id: id,
            last_id: id,
            min_id: id,
            max_id: id,
            count: 0,
            records: Vec::new(),
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
            None => write_record(&element, id, &mut c.records),
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
    if let Some((_, Some(payload))) = full {
        check_way_member_tail(payload, content.as_ref().map_or(0, |c| c.count))?;
    }
    Ok(content)
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
    kinds: [(u64, Xxh3); 3],
}

impl DigestState {
    fn new(path: Arc<Path>, hash_all_records: bool) -> Self {
        Self {
            path,
            last: None,
            all: hash_all_records.then(Xxh3::new),
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
        if let Some(all) = &mut self.all {
            all.update(&content.records);
        }
        let (count, hasher) = &mut self.kinds[usize::from(content.kind.ordinal())];
        *count += content.count;
        hasher.update(&content.records);
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
        let [nodes, ways, relations] = self.kinds.map(|(count, hasher)| KindDigest {
            count,
            hash: hasher.digest128(),
        });
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
    use super::{check_way_member_tail, way_member_bit};

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
        assert!(
            way_member_bit(payload, 9).is_err(),
            "more ways than counted"
        );
        assert!(way_member_bit(None, 0).is_err(), "declared but absent");
        assert!(check_way_member_tail(payload, 9).is_ok());
        assert!(check_way_member_tail(payload, 8).is_err(), "fewer ways");
        let padded = [0u8, 0b0000_0010];
        assert!(
            check_way_member_tail(Some((9, &padded[..])), 9).is_err(),
            "a padding bit set"
        );
        assert!(check_way_member_tail(Some((8, &[0xFF][..])), 8).is_ok());
    }
}
