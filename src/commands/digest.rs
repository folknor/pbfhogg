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

use xxhash_rust::xxh3::Xxh3;

use crate::blob_meta::{BlobIndex, ElemKind};
use crate::content::write_record;
use crate::read::header_walker::HeaderWalker;
use crate::{BlockType, Element, ElementReader, PrimitiveBlock};

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

/// Compute the element-content digest of `path`, or refuse it.
///
/// Returns an error, and no digest, for unreadable input and for any input
/// outside the domain described in the module documentation.
#[hotpath::measure]
pub fn digest(path: &Path, direct_io: bool, host: &crate::host::Host) -> Result<ContentDigest> {
    crate::debug::emit_marker("DIGEST_ENVELOPE_START");
    let indexes = walk_envelope(path, host)?;
    crate::debug::emit_marker("DIGEST_ENVELOPE_END");

    let reader = ElementReader::open_in(path, direct_io, host)?;
    if !reader.header().is_sorted() {
        return Err(format!(
            "{}: the header does not claim Sort.Type_then_ID; digest covers sorted input only, \
             as diff does. Sort it first with `pbfhogg sort`.",
            path.display()
        )
        .into());
    }

    crate::debug::emit_marker("DIGEST_SCAN_START");
    let mut state = DigestState::new(indexes);
    reader.for_each_fused_block(summarize_block, |summary| {
        state.absorb(summary).map_err(|msg| {
            crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(msg)))
        })
    })?;
    let digest = state.finish()?;
    crate::debug::emit_marker("DIGEST_SCAN_END");
    Ok(digest)
}

/// Check the blob envelope (domain rule 1, minus the sorted claim, which the
/// reader's decoded header answers) and collect each data blob's parsed
/// index in file order.
fn walk_envelope(path: &Path, host: &crate::host::Host) -> Result<Vec<Option<BlobIndex>>> {
    use crate::blob::BlobKind;

    let mut walker = HeaderWalker::open(path, &host.diagnostics)?;
    let mut indexes = Vec::new();
    let mut seen_header = false;
    while let Some(meta) = walker.next_header()? {
        match meta.blob_type {
            BlobKind::OsmHeader if !seen_header => seen_header = true,
            BlobKind::OsmHeader => {
                return Err("a second OsmHeader blob follows the first; digest accepts \
                            exactly one header, as the first blob"
                    .into());
            }
            BlobKind::OsmData if seen_header => indexes.push(meta.index),
            BlobKind::OsmData => {
                return Err("the first blob is not an OsmHeader".into());
            }
            BlobKind::Unknown(name) => {
                return Err(format!("blob of unknown type {name:?}; digest refuses it").into());
            }
        }
    }
    if !seen_header {
        return Err("no OsmHeader blob: the file is empty or not a PBF".into());
    }
    Ok(indexes)
}

/// What one decoded data block contributes, computed on a decode worker.
struct BlockSummary {
    /// `None` for a block with no elements.
    content: Option<BlockContent>,
}

struct BlockContent {
    kind: ElemKind,
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

fn element_kind(element: &Element<'_>) -> ElemKind {
    match element {
        Element::DenseNode(_) | Element::Node(_) => ElemKind::Node,
        Element::Way(_) => ElemKind::Way,
        Element::Relation(_) => ElemKind::Relation,
    }
}

fn block_kind(block_type: BlockType) -> std::result::Result<Option<ElemKind>, String> {
    match block_type {
        BlockType::DenseNodes | BlockType::Nodes => Ok(Some(ElemKind::Node)),
        BlockType::Ways => Ok(Some(ElemKind::Way)),
        BlockType::Relations => Ok(Some(ElemKind::Relation)),
        BlockType::Empty => Ok(None),
        BlockType::Mixed => Err("a data block mixes element kinds (or carries an unknown \
                                 group field); diff's streaming merge rejects such blocks"
            .to_owned()),
    }
}

/// Validate one block internally (domain rules 2 and 4) and serialize it.
/// Runs on the decode workers. Takes the block by value because the fused
/// pipeline hands ownership to its transform.
#[allow(clippy::needless_pass_by_value)]
fn summarize_block(block: PrimitiveBlock) -> std::result::Result<BlockSummary, String> {
    let classified = block_kind(block.block_type())?;
    let mut content: Option<BlockContent> = None;
    for element in block.elements() {
        let kind = element_kind(&element);
        if classified != Some(kind) {
            return Err(format!(
                "a data block classified as {} holds a {} element; diff's streaming merge \
                 would misroute it",
                classified.map_or("empty", kind_name),
                kind_name(kind),
            ));
        }
        let id = crate::osc::merge_join::element_id(&element);
        check_user(&element).map_err(|e| format!("{} {id}: {e}", kind_name(kind)))?;
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
            return Err(format!(
                "{} {id} follows {} {} in the same block; IDs must strictly ascend",
                kind_name(kind),
                kind_name(kind),
                c.last_id,
            ));
        }
        write_record(&element, id, &mut c.records)
            .map_err(|e| format!("{} {id}: {e}", kind_name(kind)))?;
        c.last_id = id;
        c.min_id = c.min_id.min(id);
        c.max_id = c.max_id.max(id);
        c.count += 1;
    }
    Ok(BlockSummary { content })
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

fn kind_name(kind: ElemKind) -> &'static str {
    match kind {
        ElemKind::Node => "node",
        ElemKind::Way => "way",
        ElemKind::Relation => "relation",
    }
}

fn kind_order(kind: ElemKind) -> u8 {
    match kind {
        ElemKind::Node => 0,
        ElemKind::Way => 1,
        ElemKind::Relation => 2,
    }
}

/// The in-order consumer: cross-block checks (domain rules 2 and 3) and the
/// streaming hash states. Nothing leaves it until [`Self::finish`], so a
/// refusal anywhere in the file prints no digest at all.
struct DigestState {
    indexes: std::vec::IntoIter<Option<BlobIndex>>,
    /// Whether the data blobs carry indexdata, fixed by the first one.
    indexed: Option<bool>,
    /// Kind and ID of the last element seen.
    last: Option<(ElemKind, i64)>,
    all: Xxh3,
    kinds: [(u64, Xxh3); 3],
}

impl DigestState {
    fn new(indexes: Vec<Option<BlobIndex>>) -> Self {
        Self {
            indexes: indexes.into_iter(),
            indexed: None,
            last: None,
            all: Xxh3::new(),
            kinds: [(0, Xxh3::new()), (0, Xxh3::new()), (0, Xxh3::new())],
        }
    }

    fn absorb(&mut self, summary: BlockSummary) -> std::result::Result<(), String> {
        // The pipeline and the envelope walk both visit data blobs in file
        // order, so the n-th summary belongs to the n-th walked blob.
        let index = self
            .indexes
            .next()
            .ok_or("the file has more data blobs than its envelope walk found")?;
        self.check_index(index, summary.content.as_ref())?;
        let Some(content) = summary.content else {
            return Ok(());
        };
        if let Some((last_kind, last_id)) = self.last {
            let (this, prev) = (kind_order(content.kind), kind_order(last_kind));
            if this < prev {
                return Err(format!(
                    "a {} block follows {} elements; kinds must appear nodes, ways, relations",
                    kind_name(content.kind),
                    kind_name(last_kind),
                ));
            }
            if this == prev && crate::osm_id::osm_id_cmp(last_id, content.first_id).is_ge() {
                return Err(format!(
                    "{} {} follows {} {} in an earlier block; IDs must strictly ascend",
                    kind_name(content.kind),
                    content.first_id,
                    kind_name(last_kind),
                    last_id,
                ));
            }
        }
        self.last = Some((content.kind, content.last_id));
        self.all.update(&content.records);
        let (count, hasher) = &mut self.kinds[usize::from(kind_order(content.kind))];
        *count += content.count;
        hasher.update(&content.records);
        Ok(())
    }

    fn check_index(
        &mut self,
        index: Option<BlobIndex>,
        content: Option<&BlockContent>,
    ) -> std::result::Result<(), String> {
        let present = index.is_some();
        match self.indexed {
            None => self.indexed = Some(present),
            Some(expected) if expected != present => {
                return Err("indexdata is present on some data blobs and absent (or \
                            unreadable) on others; diff refuses a partially indexed file"
                    .into());
            }
            Some(_) => {}
        }
        let Some(index) = index else {
            return Ok(());
        };
        let Some(c) = content else {
            return Err("an empty data block carries indexdata".into());
        };
        if index.kind != c.kind
            || index.min_id != c.min_id
            || index.max_id != c.max_id
            || index.count != c.count
        {
            return Err(format!(
                "a {} blob's indexdata claims kind {}, ids {}..={}, count {}, but the block \
                 holds ids {}..={}, count {}; diff trusts these claims",
                kind_name(c.kind),
                kind_name(index.kind),
                index.min_id,
                index.max_id,
                index.count,
                c.min_id,
                c.max_id,
                c.count,
            ));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<ContentDigest> {
        if self.indexes.next().is_some() {
            return Err("the envelope walk found more data blobs than were decoded".into());
        }
        let [nodes, ways, relations] = self.kinds;
        let kind = |(count, hasher): (u64, Xxh3)| KindDigest {
            count,
            hash: hasher.digest128(),
        };
        Ok(ContentDigest {
            all: self.all.digest128(),
            nodes: kind(nodes),
            ways: kind(ways),
            relations: kind(relations),
        })
    }
}
