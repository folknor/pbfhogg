//! Element-content digest of a PBF: `pbfhogg digest`.
//!
//! Two PBFs that the digest accepts get the same digest exactly when
//! `pbfhogg diff --suppress-common` reports them identical, barring a 128-bit
//! hash collision. The digest is independent of encoding: compression, blob
//! boundaries, dense versus plain nodes, string-table order, granularity,
//! header fields and indexdata presence all leave it unchanged. What it
//! covers is defined in one place, [`crate::content`], which is also what
//! every `diff` path compares with.
//!
//! # The domain
//!
//! `diff`'s answer is a function of element content only on inputs where its
//! optimised paths are sound, so the digest refuses everything else: it
//! returns an error and prints nothing. Refusal is the safe direction for a
//! cache built on the digest - it falls back to running the real
//! comparison. Accepted inputs satisfy all of:
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
//!
//! The promise is "refuses input that fails the validation above", not
//! "refuses every corrupt file". In particular a file truncated exactly at a
//! blob boundary is a valid shorter file: the format has no end marker.
//!
//! # Output
//!
//! [`ContentDigest::write_report`] prints one line naming the algorithm and
//! the canonical-form version, then one line per element kind with its count
//! and the digest of that kind alone:
//!
//! ```text
//! xxh128 v1 <32 hex digits>
//! node <count> <32 hex digits>
//! way <count> <32 hex digits>
//! relation <count> <32 hex digits>
//! ```
//!
//! Each digest is XXH3-128 over the canonical record stream
//! ([`crate::content::write_record`]) of the elements it covers, in file
//! order. [`CANONICAL_VERSION`] changes whenever that stream or the content
//! definition changes, so a stored digest is only ever compared with one of
//! the same version.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use xxhash_rust::xxh3::Xxh3;

use crate::blob::{Blob, BlobType};
use crate::blob_meta::BlobIndex;
use crate::content::write_record;
use crate::error::{Error, Location};
use crate::read::header_walker::{PartialIndexGuard, partial_index_error};
use crate::read::input::InputSource;
use crate::read::pipeline::DecodeTransform;
use crate::{BlockType, ByteOffset, Element, ElementKind, ElementReader, PrimitiveBlock};

use super::Result;

/// The hash algorithm, as printed on the first report line.
pub const ALGORITHM: &str = "xxh128";

/// Version of the canonical form: the content definition plus the record
/// serialization. Bump it with any change to either.
pub const CANONICAL_VERSION: u32 = 1;

/// Element count and digest for one element kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindDigest {
    pub count: u64,
    pub hash: u128,
}

/// The digest of a PBF's element content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentDigest {
    /// Digest over every element, nodes then ways then relations.
    pub all: u128,
    pub nodes: KindDigest,
    pub ways: KindDigest,
    pub relations: KindDigest,
}

impl ContentDigest {
    /// Write the report: the algorithm line, then one line per kind.
    pub fn write_report(&self, w: &mut impl Write) -> std::io::Result<()> {
        writeln!(w, "{ALGORITHM} v{CANONICAL_VERSION} {:032x}", self.all)?;
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

/// Compute the element-content digest of `input`, or refuse it.
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
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<ContentDigest> {
    let input = input.into();
    host.cancel.decided(|| digest_inner(input, direct_io, host))
}

fn digest_inner(
    input: InputSource<'_>,
    direct_io: bool,
    host: &crate::host::Host,
) -> Result<ContentDigest> {
    let path = input.path();
    // The one open of the input. The leading-header rule (domain rule 1) is
    // the reader's: it refuses a file whose first blob is not an OsmHeader,
    // and any later OsmHeader blob.
    let reader = ElementReader::open_in(input, direct_io, host)?;
    if !reader.header().is_sorted() {
        return Err(crate::error::new_unsorted(
            "the header does not claim Sort.Type_then_ID; digest covers sorted input only, \
             as diff does.",
        )
        .locate(Location::in_file(path)));
    }

    crate::debug::emit_marker("DIGEST_SCAN_START");
    let mut state = DigestState::new(Arc::from(path));
    reader.for_each_fused_blob(DigestTransform, |summary| state.absorb(summary))?;
    let digest = state.finish();
    crate::debug::emit_marker("DIGEST_SCAN_END");
    Ok(digest)
}

/// The decode-worker side of the pass: refuses a blob of unknown type
/// (domain rule 1) and summarizes each data blob with its indexdata and
/// provenance, which the ordered consumer checks.
struct DigestTransform;

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
        Some(decode().and_then(|block| {
            Ok(BlockSummary {
                index: blob.index(),
                blob: blob.blob_index(),
                offset: blob.offset(),
                content: summarize_block(block)?,
            })
        }))
    }
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
fn summarize_block(block: PrimitiveBlock) -> crate::Result<Option<BlockContent>> {
    let classified = block_kind(block.block_type())?;
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
        write_record(&element, id, &mut c.records)
            .map_err(|e| e.context(format!("{} {id}", kind.as_str())))?;
        c.last_id = id;
        c.min_id = c.min_id.min(id);
        c.max_id = c.max_id.max(id);
        c.count += 1;
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
    /// The input, to locate the refusals raised here.
    path: Arc<Path>,
    /// Domain rule 3's either-or, over the data blobs in file order. A
    /// partially indexed file is refused with the missing-indexdata error
    /// located at its first unindexed data blob (as the header walks of
    /// the commands that plan from indexdata refuse it), at the first data
    /// blob that shows the mixture; a wholly unindexed one is digested.
    partial: PartialIndexGuard<(Option<u64>, Option<ByteOffset>)>,
    /// Kind and ID of the last element seen.
    last: Option<(ElementKind, i64)>,
    all: Xxh3,
    kinds: [(u64, Xxh3); 3],
}

impl DigestState {
    fn new(path: Arc<Path>) -> Self {
        Self {
            path,
            partial: PartialIndexGuard::default(),
            last: None,
            all: Xxh3::new(),
            kinds: [(0, Xxh3::new()), (0, Xxh3::new()), (0, Xxh3::new())],
        }
    }

    /// Absorb one data blob's summary, in file order. A refusal is located
    /// at the blob that raised it.
    fn absorb(&mut self, summary: BlockSummary) -> crate::Result<()> {
        let (blob, offset) = (summary.blob, summary.offset);
        if let Some((blob, offset)) = self
            .partial
            .observe(summary.index.is_some(), (blob, offset))
        {
            return Err(partial_index_error().locate(Location::new(
                Some(&self.path),
                blob,
                offset,
            )));
        }
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
        self.all.update(&content.records);
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

    fn finish(self) -> ContentDigest {
        let [nodes, ways, relations] = self.kinds;
        let kind = |(count, hasher): (u64, Xxh3)| KindDigest {
            count,
            hash: hasher.digest128(),
        };
        ContentDigest {
            all: self.all.digest128(),
            nodes: kind(nodes),
            ways: kind(ways),
            relations: kind(relations),
        }
    }
}
