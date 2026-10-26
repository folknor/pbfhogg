//! `check --index`: verify a PBF's blob-level sidecar metadata against the
//! blobs themselves.
//!
//! # Why this exists
//!
//! `BlobHeader.indexdata` (field 2) and `BlobHeader.tagdata` (field 4) are
//! *hints*: they claim an element kind, an ID range, an element count, a
//! spatial bbox, and a tag key set for a blob whose payload the reader has not
//! decompressed yet. Every fast path in this crate believes them - `cat --type`
//! passes a blob through raw on the strength of `kind`, `renumber` assigns IDs
//! from `count`, `tags-filter` skips whole blobs on the strength of the tag
//! index, and the spatial filter skips node blobs on the strength of the bbox.
//!
//! Nothing in the format signs those claims, and until this command there was
//! no way to ask whether they were true. `--distrust-metadata` lets a consumer
//! decline to believe them, and `degrade --corrupt-indexdata` /
//! `--corrupt-indexdata-kind` / `--corrupt-tagdata` can manufacture a file whose
//! hints are well-formed and false - but the pair was one-sided. This is the
//! other side: it decompresses every blob, re-derives the metadata from the
//! payload with [`crate::blob_meta::try_scan_block_ids_checked`] /
//! [`scan_block_tags`], and diffs. Those are
//! the writer's *passthrough*-path scanners; the ordinary write path derives
//! its index incrementally inside `BlockBuilder` and never rescans what it
//! emitted, so against it this check is genuinely independent.
//!
//! # Absence, and what is not absence
//!
//! Metadata a blob does not carry is not a claim, and is not reported - that is
//! the ordinary state of any third-party PBF. Metadata a blob carries but that
//! cannot be read is a different fact and *is* reported, which is a distinction
//! no reader in this crate makes: `BlobIndex::deserialize` and
//! `TagIndex::deserialize` both return `None` for unreadable bytes, and every
//! reader is right to treat that as "no metadata, decode the blob". Inheriting
//! that collapse is how a verifier ends up passing the most obviously broken
//! input it can be handed, so this module reads the raw field's presence
//! separately from its parsed value. The same reasoning covers a well-formed
//! tag index attached to a blob whose group mixes element kinds: the scanner
//! refuses such a blob precisely because it can only see the first element
//! field, so any key set over it is incomplete - and `tags-filter` skips whole
//! blobs on that key set.
//!
//! # What "distrust" means here
//!
//! There is no `--distrust-metadata` flag on this command, and adding one would
//! be meaningless: a verifier that consults the thing it is verifying in order
//! to decide what to look at is worthless. This command never routes on
//! indexdata. It installs no [`BlobFilter`](crate::BlobFilter), never calls
//! `require_indexdata`, and decompresses every OSMData blob unconditionally,
//! including blobs whose stored index says they hold a kind the caller did not
//! ask about. The metadata is input under test, never a routing hint.
//!
//! # Exact match versus soundness
//!
//! Two of the checks are containment rather than equality, and deliberately so.
//!
//! * **bbox** - the scanner derives its box from the blob's raw coordinates
//!   through the block's `granularity`/`lat_offset`/`lon_offset`, rounding
//!   outward (floor for the minima, ceil for the maxima). On a block with a
//!   non-default granularity that rounding makes the *scanned* box legitimately
//!   wider than the exact box a writer with the true coordinates could store.
//!   Demanding equality would therefore report an honest third-party file as
//!   lying. What actually matters is soundness: a stored box must contain
//!   every coordinate in the blob, or the spatial filter skips a blob holding
//!   nodes inside the query region. So the test is containment - and an
//!   *absent* box is no claim at all rather than an infinitely small one,
//!   because `BlobFilter::wants_index` passes a boxless blob straight through.
//!   (v1 indexdata has no bbox field, so treating absence as a violation fails
//!   on every v1 file, which is what the first Denmark run of this command
//!   demonstrated.) The coordinates a box claims to contain are the exact
//!   `offset + granularity * raw` nanodegrees; one outside the `i32`
//!   decimicrodegree range, where the scan has no box, is outside every
//!   stored box, and is reported as such. The `sort` / `cat --dedupe` merge
//!   engine keeps indexdata by the same rule (`BlobIndex::bbox_claim`).
//! * **tag keys** - a stored key set that *omits* a key the blob carries makes
//!   `tags-filter` skip a blob that does contain matches, which is a wrong
//!   answer. A stored key set that lists a key the blob does not carry only
//!   costs a needless decode. Both are false statements about the file and both
//!   are reported, but they are reported as distinct violations so the output
//!   says which one it is.
//!
//! The kind, count and ID-range checks are exact. Every consumer of those
//! treats them as exact, so anything but equality is a lie with a consumer.

use crate::blob::BlobKind;
use crate::blob_meta::{BboxClaim, BlobBbox, BlobIndex, scan_block_tags};
use crate::error::Result;
use crate::file_reader::FileReader;
use crate::read::decompress::decompress_blob_data_into;
use crate::read::frame::Frames;
use crate::read::input::InputSource;

/// Compatibility name for the options shared by all check modes.
pub type CheckIndexOptions = super::CheckOptions;

/// One way a blob's stored metadata disagrees with the blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexViolationKind {
    /// `indexdata.kind` names a different element kind than the blob holds.
    Kind {
        claimed: &'static str,
        observed: &'static str,
    },
    /// `indexdata.count` differs from the number of elements in the blob.
    Count { claimed: u64, observed: u64 },
    /// `indexdata.min_id` differs from the blob's smallest element ID.
    MinId { claimed: i64, observed: i64 },
    /// `indexdata.max_id` differs from the blob's largest element ID.
    MaxId { claimed: i64, observed: i64 },
    /// The stored bbox is present and does not contain every coordinate in the
    /// blob, so a spatial filter can skip a blob holding nodes inside the query
    /// region. An *absent* bbox is not this: consumers read absence as "no
    /// spatial information" and decode the blob.
    BboxTooSmall {
        claimed: [i32; 4],
        observed: [i32; 4],
    },
    /// The stored bbox is present over a node blob with a coordinate outside
    /// the `i32` decimicrodegree range (about +-214.7 degrees), which no box
    /// can contain, so the claim is false. Far enough out, the decoder's
    /// accessors also wrap such a coordinate to an arbitrary position the
    /// box need not cover, where a spatial filter can skip a blob holding a
    /// node a decode would place in the query region.
    BboxOutOfRange { claimed: [i32; 4] },
    /// The header carries an indexdata for a well-formed blob this crate
    /// refuses to index at all (a group mixing element kinds). A malformed
    /// payload is not this: `check_index` fails with an error. Whatever
    /// the claim says, it cannot be checked and cannot have been produced
    /// honestly by this crate.
    IndexOnUnindexableBlob,
    /// The header carries an `indexdata` field whose bytes this crate cannot
    /// read - an unrecognised length, an unknown version byte, or an
    /// out-of-range element-kind byte.
    ///
    /// Every *reader* collapses this to "no index" and decodes the blob, which
    /// is the safe direction, and that is exactly why the verifier has to
    /// report it: the one input a verifier most obviously must reject was
    /// otherwise indistinguishable from a file that made no claim at all.
    MalformedIndexdata,
    /// The header carries a `tagdata` field whose bytes are not a valid tag
    /// index. Readers silently ignore it, which costs nothing but means the
    /// file is not what it says it is.
    MalformedTagdata,
    /// The header carries a valid, readable `tagdata` for a well-formed blob
    /// this crate refuses to index at all (a group mixing element kinds).
    ///
    /// This is not cosmetic. A mixed-kind group makes `scan_block_tags` return
    /// `None` precisely because the scanner would see only the first element
    /// field, so any tag index over such a blob is at best incomplete - and
    /// `tags-filter` skips whole blobs on that index, dropping the matches it
    /// cannot see. A blob that carries `amenity` behind a valid-looking index
    /// that omits it is a wrong answer with no error anywhere.
    TagdataOnUnindexableBlob,
    /// The stored tag index omits keys the blob carries. `tags-filter` skips
    /// blobs on this index, so an omission drops matches.
    TagKeysMissing { missing: usize, example: String },
    /// The stored tag index lists keys the blob does not carry. Sound but
    /// false: it costs a decode that the index existed to avoid.
    TagKeysSpurious { spurious: usize, example: String },
    /// Some data blobs carry indexdata and this one, which could carry it,
    /// does not. Consumers that read indexdata from the first blob alone take
    /// the file as indexed and then fail or misbehave at this blob. A wholly
    /// unindexed file is not this.
    MissingAmongIndexed,
}

/// A single disagreement, located in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexViolation {
    /// Ordinal of the OSMData blob within the file, counting from 0.
    pub blob_ordinal: u64,
    /// Byte offset of the blob's frame in the file.
    pub file_offset: u64,
    pub kind: IndexViolationKind,
}

impl std::fmt::Display for IndexViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "blob {} (offset {}): ",
            self.blob_ordinal, self.file_offset
        )?;
        match &self.kind {
            IndexViolationKind::Kind { claimed, observed } => {
                write!(f, "indexdata claims kind {claimed}, blob holds {observed}")
            }
            IndexViolationKind::Count { claimed, observed } => {
                write!(
                    f,
                    "indexdata claims {claimed} elements, blob holds {observed}"
                )
            }
            IndexViolationKind::MinId { claimed, observed } => {
                write!(
                    f,
                    "indexdata claims min_id {claimed}, blob's smallest ID is {observed}"
                )
            }
            IndexViolationKind::MaxId { claimed, observed } => {
                write!(
                    f,
                    "indexdata claims max_id {claimed}, blob's largest ID is {observed}"
                )
            }
            IndexViolationKind::BboxTooSmall {
                claimed: c,
                observed,
            } => write!(
                f,
                "indexdata bbox [{},{},{},{}] does not contain the blob's coordinates \
                 [{},{},{},{}]",
                c[0], c[1], c[2], c[3], observed[0], observed[1], observed[2], observed[3]
            ),
            IndexViolationKind::BboxOutOfRange { claimed: c } => write!(
                f,
                "indexdata bbox [{},{},{},{}] cannot contain the blob's coordinates: \
                 some lie outside the decimicrodegree range",
                c[0], c[1], c[2], c[3]
            ),
            IndexViolationKind::IndexOnUnindexableBlob => write!(
                f,
                "indexdata is present for a blob that cannot be indexed \
                 (e.g. a mixed-kind group)"
            ),
            IndexViolationKind::MalformedIndexdata => write!(
                f,
                "indexdata is present but is not a readable blob index \
                 (unrecognised length, version or element kind)"
            ),
            IndexViolationKind::MalformedTagdata => {
                write!(f, "tagdata is present but is not a valid tag index")
            }
            IndexViolationKind::TagdataOnUnindexableBlob => write!(
                f,
                "tagdata is present for a blob that cannot be indexed \
                 (e.g. a mixed-kind group), so the key set \
                 cannot be complete"
            ),
            IndexViolationKind::TagKeysMissing { missing, example } => write!(
                f,
                "tagdata omits {missing} key(s) the blob carries (e.g. {example:?})"
            ),
            IndexViolationKind::TagKeysSpurious { spurious, example } => write!(
                f,
                "tagdata lists {spurious} key(s) the blob does not carry (e.g. {example:?})"
            ),
            IndexViolationKind::MissingAmongIndexed => write!(
                f,
                "no indexdata, but other blobs in this file carry it: the file is \
                 partially indexed (first such blob shown)"
            ),
        }
    }
}

/// Result of an index verification pass.
#[derive(Debug, Clone, Default)]
pub struct IndexReport {
    /// OSMData blobs seen.
    pub blobs: u64,
    /// Blobs whose header carried a parseable `indexdata`.
    pub blobs_with_index: u64,
    /// Blobs whose header carried a `tagdata` field.
    pub blobs_with_tagdata: u64,
    /// Blobs this crate declines to index at all. Reported for context: a
    /// blob here without stored metadata is legal, one *with* stored metadata
    /// is a violation.
    pub blobs_unindexable: u64,
    /// Stored violations, limited by `max_errors` unless it is zero.
    pub violations: Vec<IndexViolation>,
    /// Total violations found, which may exceed `violations.len()`.
    pub total_violations: u64,
}

impl IndexReport {
    /// True when every stored claim matched the blob it describes.
    pub fn passed(&self) -> bool {
        self.total_violations == 0
    }

    /// Build the report as a JSON value.
    pub fn to_json_value(&self, file_name: &str) -> serde_json::Value {
        let violations: Vec<serde_json::Value> = self
            .violations
            .iter()
            .map(|v| {
                serde_json::json!({
                    "blob": v.blob_ordinal,
                    "offset": v.file_offset,
                    "detail": v.to_string(),
                })
            })
            .collect();
        serde_json::json!({
            "file": file_name,
            "blobs": self.blobs,
            "blobs_with_indexdata": self.blobs_with_index,
            "blobs_with_tagdata": self.blobs_with_tagdata,
            "blobs_unindexable": self.blobs_unindexable,
            "passed": self.passed(),
            "total_violations": self.total_violations,
            "violations": violations,
        })
    }

    /// Write the human-readable summary to `out`.
    ///
    /// Rendered into a caller-supplied writer rather than the process's
    /// stdout so a write failure - the reader of a pipe going away - comes
    /// back as an error instead of a `println!` panic.
    ///
    /// # Errors
    ///
    /// Returns the first error `out` reports.
    pub fn write_human<W: std::io::Write>(
        &self,
        out: &mut W,
        file_name: &str,
    ) -> std::io::Result<()> {
        for v in &self.violations {
            writeln!(out, "{v}")?;
        }
        if self.total_violations > self.violations.len() as u64 {
            writeln!(
                out,
                "... {} further violation(s) not shown (the report cap was reached)",
                self.total_violations - self.violations.len() as u64
            )?;
        }
        writeln!(
            out,
            "Blobs: {} ({} with indexdata, {} with tagdata, {} unindexable)",
            self.blobs, self.blobs_with_index, self.blobs_with_tagdata, self.blobs_unindexable
        )?;
        if self.passed() {
            writeln!(out, "Blob metadata for {file_name}: OK")
        } else {
            writeln!(
                out,
                "Blob metadata for {file_name}: FAILED ({} violations)",
                self.total_violations
            )
        }
    }
}

fn bbox_array(b: &BlobBbox) -> [i32; 4] {
    [b.min_lat, b.max_lat, b.min_lon, b.max_lon]
}

/// Compare one blob's stored metadata against what the blob actually holds.
///
/// Split out from the read loop so the comparison is unit-testable against
/// hand-built `(stored, observed)` pairs without going through a file.
fn diff_blob(
    stored: Option<&BlobIndex>,
    stored_present: bool,
    observed: Option<&BlobIndex>,
    out: &mut Vec<IndexViolationKind>,
) {
    let Some(stored) = stored else {
        // `stored` is `None` for two very different files: one that carries no
        // indexdata (legal, and what every third-party PBF looks like) and one
        // that carries indexdata whose bytes cannot be read. The read path
        // cannot tell them apart because it does not need to - both mean
        // "decode the blob" - but a verifier that cannot tell them apart passes
        // the single most obviously broken input there is.
        if stored_present {
            out.push(IndexViolationKind::MalformedIndexdata);
        }
        return;
    };
    let Some(observed) = observed else {
        out.push(IndexViolationKind::IndexOnUnindexableBlob);
        return;
    };
    if stored.kind != observed.kind {
        out.push(IndexViolationKind::Kind {
            claimed: stored.kind.as_str(),
            observed: observed.kind.as_str(),
        });
    }
    if stored.count != observed.count {
        out.push(IndexViolationKind::Count {
            claimed: stored.count,
            observed: observed.count,
        });
    }
    if stored.min_id != observed.min_id {
        out.push(IndexViolationKind::MinId {
            claimed: stored.min_id,
            observed: observed.min_id,
        });
    }
    if stored.max_id != observed.max_id {
        out.push(IndexViolationKind::MaxId {
            claimed: stored.max_id,
            observed: observed.max_id,
        });
    }
    // Containment, not equality - see the module docs. The rule is shared
    // with the merge engine, which keeps a blob's indexdata exactly when this
    // check would report nothing against it.
    //
    // An *absent* stored bbox is not a violation, and getting that wrong was
    // the first thing the Denmark real-data run caught: every one of the file's
    // 6562 node blobs carries a v1 (26-byte) index, which has no bbox field at
    // all, and reporting them made the command fail on an honest file. Absence
    // is not a false claim - `BlobFilter::wants_index` treats a missing bbox as
    // "no spatial information, decode the blob", so a stored index without a
    // box cannot cause a wrong answer. Only a box that is *present* and *too
    // small* can, because that is the one shape a spatial filter acts on; a
    // node coordinate outside the decimicrodegree range is outside every box.
    match stored.bbox_claim(observed) {
        BboxClaim::Absent | BboxClaim::NotNodes | BboxClaim::Holds => {}
        BboxClaim::TooSmall { claimed, observed } => {
            out.push(IndexViolationKind::BboxTooSmall {
                claimed: bbox_array(&claimed),
                observed: bbox_array(&observed),
            });
        }
        BboxClaim::OutOfRange { claimed } => {
            out.push(IndexViolationKind::BboxOutOfRange {
                claimed: bbox_array(&claimed),
            });
        }
    }
}

/// Compare one blob's stored tag index against the keys the blob carries.
///
/// `stored_bytes` is the raw `BlobHeader.tagdata`. Absent tagdata makes no
/// claim and is not a violation - readers treat it as "decode this blob".
fn diff_tagdata(stored_bytes: Option<&[u8]>, block: &[u8], out: &mut Vec<IndexViolationKind>) {
    let Some(bytes) = stored_bytes else { return };
    let Some(stored) = crate::blob_meta::TagIndex::deserialize(bytes) else {
        out.push(IndexViolationKind::MalformedTagdata);
        return;
    };
    let Some(observed) = scan_block_tags(block) else {
        // The blob is one no tag index can describe (a mixed-kind group; a
        // malformed payload already failed the ID scan in `check_index`), yet
        // a well-formed tag index was stored for it.
        //
        // An earlier draft returned here without reporting, on the reasoning
        // that `scan_block_ids` refuses the same blob and so
        // `IndexOnUnindexableBlob` would already have fired. That reasoning is
        // wrong twice over. It only holds when indexdata is *also* present -
        // tagdata is an independent field and a blob may carry it alone - and
        // even when both fire, they are two separate false claims about the
        // file. The hole it left is the exact silently-wrong-output shape this
        // command exists to close: a mixed-kind blob containing `amenity`,
        // carrying a valid-but-incomplete tag index, passing `check --index`,
        // and then being skipped by a `tags-filter -e amenity=...` that
        // believes the index.
        out.push(IndexViolationKind::TagdataOnUnindexableBlob);
        return;
    };
    let stored_keys = stored.keys();
    let observed_keys = observed.keys();

    let missing: Vec<&[u8]> = observed_keys
        .iter()
        .filter(|k| stored_keys.binary_search(*k).is_err())
        .map(std::convert::AsRef::as_ref)
        .collect();
    if let Some(first) = missing.first() {
        out.push(IndexViolationKind::TagKeysMissing {
            missing: missing.len(),
            example: String::from_utf8_lossy(first).into_owned(),
        });
    }
    let spurious: Vec<&[u8]> = stored_keys
        .iter()
        .filter(|k| observed_keys.binary_search(*k).is_err())
        .map(std::convert::AsRef::as_ref)
        .collect();
    if let Some(first) = spurious.first() {
        out.push(IndexViolationKind::TagKeysSpurious {
            spurious: spurious.len(),
            example: String::from_utf8_lossy(first).into_owned(),
        });
    }
}

/// Decompress every OSMData blob and diff its payload against the metadata its
/// `BlobHeader` claims.
///
/// Reads the input front to back through one open, so it takes no pin of its
/// own and reads a FIFO; a caller that composes it with other checks on the
/// same input pins once and hands the pin here.
pub fn check_index<'a>(
    input: impl Into<InputSource<'a>>,
    opts: &CheckIndexOptions,
    host: &crate::host::Host,
) -> Result<IndexReport> {
    let input = input.into();
    host.cancel.decided(|| check_index_inner(input, opts, host))
}

fn check_index_inner(
    input: InputSource<'_>,
    opts: &CheckIndexOptions,
    host: &crate::host::Host,
) -> Result<IndexReport> {
    let path = input.path();
    let mut frames = Frames::of_input(FileReader::open(input, opts.direct_io)?, path, host);
    // The verifier is exempt from the command-input refusal of a partially
    // indexed file: reporting it, as a violation, is part of its job.
    frames.accept_partial_index();
    let mut block = Vec::new();
    let mut report = IndexReport::default();
    let mut found = Vec::new();
    // The first indexable blob with no indexdata, for the partial-index verdict.
    let mut first_unindexed: Option<(u64, u64)> = None;

    crate::debug::emit_marker("CHECK_INDEX_START");
    let mut first = true;
    while let Some(frame) = frames.next_raw()? {
        // The header comes first or the file is not a PBF: a zero-byte or
        // header-less file gets the one missing-header error, not a clean
        // report over data blobs it cannot place.
        if first && !matches!(frame.blob_type, BlobKind::OsmHeader) {
            return Err(crate::error::new_missing_header(path));
        }
        first = false;
        if !matches!(frame.blob_type, BlobKind::OsmData) {
            continue;
        }
        let ordinal = report.blobs;
        report.blobs += 1;
        if frame.index.is_some() {
            report.blobs_with_index += 1;
        }
        if frame.tagdata_present {
            report.blobs_with_tagdata += 1;
        }

        // Unconditional decode: the stored metadata is under test, so it must
        // not be allowed to decide which blobs get looked at.
        decompress_blob_data_into(frame.blob_bytes(), &mut block)
            .map_err(|e| frames.locate(frame.id, e))?;
        // A malformed payload is an error even when no metadata was stored.
        // Valid unsupported shapes remain unindexable rather than malformed.
        let observed = crate::blob_meta::try_scan_block_ids_checked(&block)
            .map_err(|e| {
                frames.locate(
                    frame.id,
                    e.context(format!("OsmData blob {ordinal}: malformed payload")),
                )
            })?
            .map(|(index, _sorted)| index);
        if observed.is_none() {
            report.blobs_unindexable += 1;
        } else if !frame.index_present && first_unindexed.is_none() {
            first_unindexed = Some((ordinal, frame.file_offset()));
        }

        found.clear();
        diff_blob(
            frame.index.as_ref(),
            frame.index_present,
            observed.as_ref(),
            &mut found,
        );
        if frame.tagdata_present && frame.tagdata.is_none() {
            found.push(IndexViolationKind::MalformedTagdata);
        } else {
            diff_tagdata(frame.tagdata.as_deref(), &block, &mut found);
        }
        block.clear();

        for kind in found.drain(..) {
            report.total_violations += 1;
            if opts.max_errors == 0 || report.violations.len() < opts.max_errors {
                report.violations.push(IndexViolation {
                    blob_ordinal: ordinal,
                    file_offset: frame.file_offset(),
                    kind,
                });
            }
        }
    }
    if first {
        return Err(crate::error::new_missing_header(path));
    }
    crate::debug::emit_marker("CHECK_INDEX_END");

    if let Some((blob_ordinal, file_offset)) = first_unindexed
        && report.blobs_with_index > 0
    {
        report.total_violations += 1;
        if opts.max_errors == 0 || report.violations.len() < opts.max_errors {
            report.violations.push(IndexViolation {
                blob_ordinal,
                file_offset,
                kind: IndexViolationKind::MissingAmongIndexed,
            });
        }
    }

    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ElementKind;

    fn idx(kind: ElementKind, min_id: i64, max_id: i64, count: u64) -> BlobIndex {
        BlobIndex {
            kind,
            min_id,
            max_id,
            count,
            bbox: None,
        }
    }

    #[test]
    fn an_honest_index_produces_no_violations() {
        // The passing counterpart every rejection below is paired against: if
        // `diff_blob` regressed into reporting something unconditionally, this
        // is the test that catches it.
        let stored = idx(ElementKind::Way, 10, 20, 5);
        let observed = idx(ElementKind::Way, 10, 20, 5);
        let mut out = Vec::new();
        diff_blob(Some(&stored), true, Some(&observed), &mut out);
        assert!(
            out.is_empty(),
            "an exact match must report nothing: {out:?}"
        );
    }

    #[test]
    fn absent_metadata_makes_no_claim_to_check() {
        // A blob with no indexdata is legal and extremely common (any
        // third-party PBF). Reporting it would make `check --index` fail on
        // every osmium-written file.
        let observed = idx(ElementKind::Node, 1, 2, 2);
        let mut out = Vec::new();
        diff_blob(None, false, Some(&observed), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn an_inflated_count_and_max_id_are_both_reported() {
        // Exactly the shape `degrade --corrupt-indexdata` writes: count and
        // max_id each overstated by CORRUPT_INDEXDATA_INFLATION.
        let stored = idx(ElementKind::Way, 10, 1_000_020, 1_000_005);
        let observed = idx(ElementKind::Way, 10, 20, 5);
        let mut out = Vec::new();
        diff_blob(Some(&stored), true, Some(&observed), &mut out);
        assert!(out.contains(&IndexViolationKind::Count {
            claimed: 1_000_005,
            observed: 5
        }));
        assert!(out.contains(&IndexViolationKind::MaxId {
            claimed: 1_000_020,
            observed: 20
        }));
        assert!(
            !out.iter()
                .any(|v| matches!(v, IndexViolationKind::Kind { .. })),
            "--corrupt-indexdata deliberately preserves the kind, so a kind \
             violation here would mean the comparison is not field-specific"
        );
    }

    #[test]
    fn a_rotated_kind_is_reported() {
        // `degrade --corrupt-indexdata-kind` rotates node->way->relation->node
        // and leaves every other field honest.
        let stored = idx(ElementKind::Relation, 10, 20, 5);
        let observed = idx(ElementKind::Way, 10, 20, 5);
        let mut out = Vec::new();
        diff_blob(Some(&stored), true, Some(&observed), &mut out);
        assert_eq!(
            out,
            vec![IndexViolationKind::Kind {
                claimed: "relation",
                observed: "way"
            }],
            "the kind rotation must be the only thing reported"
        );
    }

    #[test]
    fn an_index_on_an_unindexable_blob_is_reported() {
        let stored = idx(ElementKind::Way, 10, 20, 5);
        let mut out = Vec::new();
        diff_blob(Some(&stored), true, None, &mut out);
        assert_eq!(out, vec![IndexViolationKind::IndexOnUnindexableBlob]);

        // Passing counterpart: no stored index over the same unindexable blob
        // is the ordinary "this file has no indexdata" case.
        let mut out = Vec::new();
        diff_blob(None, false, None, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn the_bbox_test_is_containment_not_equality() {
        let mut stored = idx(ElementKind::Node, 1, 2, 2);
        stored.bbox = Some(BlobBbox::new(0, 100, 0, 100));

        // Wider stored box: sound, and the case a non-default granularity or a
        // conservatively-rounded scan produces. Must NOT be reported, or every
        // honest third-party file fails.
        let mut observed = idx(ElementKind::Node, 1, 2, 2);
        observed.bbox = Some(BlobBbox::new(10, 90, 10, 90));
        let mut out = Vec::new();
        diff_blob(Some(&stored), true, Some(&observed), &mut out);
        assert!(out.is_empty(), "a stored box wider than the blob is sound");

        // Narrower stored box: unsound, because the spatial filter would skip a
        // blob holding nodes inside the query region.
        observed.bbox = Some(BlobBbox::new(-10, 90, 10, 90));
        let mut out = Vec::new();
        diff_blob(Some(&stored), true, Some(&observed), &mut out);
        assert_eq!(
            out,
            vec![IndexViolationKind::BboxTooSmall {
                claimed: [0, 100, 0, 100],
                observed: [-10, 90, 10, 90],
            }]
        );
    }

    #[test]
    fn a_stored_index_with_no_bbox_is_not_a_violation() {
        // Regression pin for the first thing the Denmark real-data run caught:
        // that file's 6562 node blobs all carry a v1 (26-byte) index, which has
        // no bbox field, and an earlier draft of this command reported every
        // one of them - failing on an honest file, which is the one outcome a
        // verifier must never produce.
        //
        // Absence is not a false claim: `BlobFilter::wants_index` reads a
        // missing bbox as "no spatial information" and lets the blob through,
        // so it cannot cause a wrong answer.
        let stored = idx(ElementKind::Node, 1, 2, 2);
        assert!(stored.bbox.is_none());
        let mut observed = idx(ElementKind::Node, 1, 2, 2);
        observed.bbox = Some(BlobBbox::new(10, 90, 10, 90));
        let mut out = Vec::new();
        diff_blob(Some(&stored), true, Some(&observed), &mut out);
        assert!(
            out.is_empty(),
            "a v1 index carries no bbox and claims nothing spatial: {out:?}"
        );

        // Paired counterpart so this is not satisfied by "never report a bbox
        // violation": a bbox that IS present and IS too small must still fire.
        let mut narrow = idx(ElementKind::Node, 1, 2, 2);
        narrow.bbox = Some(BlobBbox::new(20, 90, 10, 90));
        let mut out = Vec::new();
        diff_blob(Some(&narrow), true, Some(&observed), &mut out);
        assert_eq!(
            out,
            vec![IndexViolationKind::BboxTooSmall {
                claimed: [20, 90, 10, 90],
                observed: [10, 90, 10, 90],
            }]
        );
    }

    /// A node blob the scan gives no box has a coordinate outside the
    /// decimicrodegree range, which no stored box contains.
    #[test]
    fn a_stored_bbox_over_out_of_range_coordinates_is_reported() {
        let mut stored = idx(ElementKind::Node, 1, 2, 2);
        stored.bbox = Some(BlobBbox::new(i32::MIN, i32::MAX, i32::MIN, i32::MAX));
        let boxless = idx(ElementKind::Node, 1, 2, 2);
        let mut out = Vec::new();
        diff_blob(Some(&stored), true, Some(&boxless), &mut out);
        assert_eq!(
            out,
            vec![IndexViolationKind::BboxOutOfRange {
                claimed: [i32::MIN, i32::MAX, i32::MIN, i32::MAX],
            }],
            "even the full-domain box cannot contain such a coordinate"
        );

        // Counterparts: the same boxless scan under a stored index without a
        // box claims nothing, and a stored node box over a blob that holds
        // ways is the kind finding alone.
        let mut out = Vec::new();
        diff_blob(Some(&boxless), true, Some(&boxless), &mut out);
        assert!(out.is_empty(), "no stored box is no claim: {out:?}");
        let ways = idx(ElementKind::Way, 1, 2, 2);
        let mut out = Vec::new();
        diff_blob(Some(&stored), true, Some(&ways), &mut out);
        assert_eq!(
            out,
            vec![IndexViolationKind::Kind {
                claimed: "node",
                observed: "way"
            }]
        );
    }

    #[test]
    fn malformed_tagdata_is_reported_but_absent_tagdata_is_not() {
        let mut out = Vec::new();
        diff_tagdata(Some(&[0xFF, 0x00, 0x00]), &[], &mut out);
        assert_eq!(out, vec![IndexViolationKind::MalformedTagdata]);

        let mut out = Vec::new();
        diff_tagdata(None, &[], &mut out);
        assert!(out.is_empty(), "no tagdata is no claim");
    }

    /// Indexdata the reader cannot read is a claim the file cannot back up,
    /// and it must not be excused as "this blob makes no claim".
    ///
    /// `Frames::next_raw` deserializes the raw field into an `Option<BlobIndex>`,
    /// so invalid bytes reach `diff_blob` as `None` - byte-identical, at this
    /// boundary, to a file that carries no indexdata at all. Without the
    /// separate presence flag the verifier silently passed the single most
    /// obviously broken input it can be handed.
    #[test]
    fn malformed_indexdata_is_reported_rather_than_read_as_absent() {
        let observed = idx(ElementKind::Node, 1, 2, 2);
        let mut out = Vec::new();
        diff_blob(None, true, Some(&observed), &mut out);
        assert_eq!(
            out,
            vec![IndexViolationKind::MalformedIndexdata],
            "indexdata that is present but unreadable is a violation"
        );

        // Paired counterpart, so this is not satisfied by a
        // report-everything regression: the same unreadable-index code path
        // with the field genuinely absent must stay silent, which is the case
        // every third-party PBF hits on every blob.
        let mut out = Vec::new();
        diff_blob(None, false, Some(&observed), &mut out);
        assert!(
            out.is_empty(),
            "a blob with no indexdata field makes no claim: {out:?}"
        );

        // And a readable index still gets compared field by field rather than
        // short-circuiting on the presence flag.
        let stored = idx(ElementKind::Way, 10, 20, 5);
        let mut out = Vec::new();
        diff_blob(Some(&stored), true, Some(&stored), &mut out);
        assert!(out.is_empty());
    }

    /// A `PrimitiveBlock` whose single group carries both a Way (field 3) and
    /// a Relation (field 4). Every scanner in the crate stops at the first
    /// element field, so `scan_block_tags` refuses the blob outright rather
    /// than emit a key set missing the relation's keys.
    fn mixed_kind_block() -> Vec<u8> {
        let mut st = Vec::new();
        for s in [b"".as_slice(), b"highway".as_slice(), b"amenity".as_slice()] {
            st.push(0x0A);
            st.push(u8::try_from(s.len()).unwrap());
            st.extend_from_slice(s);
        }
        // Way with keys [1], Relation with keys [2].
        let way = [0x12u8, 0x01, 0x01];
        let rel = [0x12u8, 0x01, 0x02];
        let mut group = Vec::new();
        group.push(0x1A);
        group.push(u8::try_from(way.len()).unwrap());
        group.extend_from_slice(&way);
        group.push(0x22);
        group.push(u8::try_from(rel.len()).unwrap());
        group.extend_from_slice(&rel);

        let mut block = Vec::new();
        block.push(0x0A);
        block.push(u8::try_from(st.len()).unwrap());
        block.extend_from_slice(&st);
        block.push(0x12);
        block.push(u8::try_from(group.len()).unwrap());
        block.extend_from_slice(&group);
        block
    }

    /// A valid tag index over a blob no tag index can describe is a lie with a
    /// consumer, and reporting it must not depend on indexdata also being
    /// present.
    ///
    /// This is the interaction between two separate rounds of this work: the
    /// mixed-kind guard that makes both scanners refuse such a blob, and the
    /// verifier built later. Each was right on its own; together they left a
    /// blob that can carry `amenity`, carry a valid-but-incomplete tag index,
    /// pass `check --index`, and then be skipped by a `tags-filter -e
    /// amenity=...` that believes the index.
    #[test]
    fn a_tag_index_over_an_unindexable_blob_is_reported() {
        let block = mixed_kind_block();
        assert!(
            scan_block_tags(&block).is_none(),
            "fixture must be a blob the tag scanner refuses, or this test \
             proves nothing"
        );
        let stored = crate::blob_meta::encode_tag_index(std::iter::once(b"highway".as_slice()))
            .expect("one short key is encodable");

        let mut out = Vec::new();
        diff_tagdata(Some(&stored), &block, &mut out);
        assert_eq!(
            out,
            vec![IndexViolationKind::TagdataOnUnindexableBlob],
            "a tag index over a blob that cannot be indexed must be reported \
             on its own, not only when indexdata happens to be present too"
        );

        // Paired counterpart: no tagdata over the same blob is legal and is
        // exactly what this crate writes for it, so the check must stay quiet.
        let mut out = Vec::new();
        diff_tagdata(None, &block, &mut out);
        assert!(
            out.is_empty(),
            "a mixed-kind blob with no tag index is what pbfhogg itself \
             produces: {out:?}"
        );

        // Second counterpart: the same honest tag index over a blob that CAN
        // be scanned must also stay quiet, so this is not satisfied by
        // "always report when tagdata is present".
        let mut clean = Vec::new();
        let mut st = Vec::new();
        for s in [b"".as_slice(), b"highway".as_slice()] {
            st.push(0x0A);
            st.push(u8::try_from(s.len()).unwrap());
            st.extend_from_slice(s);
        }
        let way = [0x12u8, 0x01, 0x01];
        let mut group = Vec::new();
        group.push(0x1A);
        group.push(u8::try_from(way.len()).unwrap());
        group.extend_from_slice(&way);
        clean.push(0x0A);
        clean.push(u8::try_from(st.len()).unwrap());
        clean.extend_from_slice(&st);
        clean.push(0x12);
        clean.push(u8::try_from(group.len()).unwrap());
        clean.extend_from_slice(&group);
        let mut out = Vec::new();
        diff_tagdata(Some(&stored), &clean, &mut out);
        assert!(
            out.is_empty(),
            "an honest tag index over a scannable blob must verify clean: \
             {out:?}"
        );
    }
}
