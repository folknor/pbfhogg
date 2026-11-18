//! Shared OsmData blob metadata scan for ALTW external join.
//!
//! Replaces repeated header-only scans in stage 1 and stage 4 with one
//! reusable pass over blob headers.

use crate::read::input::PinnedInput;

use crate::ElementKind;

use super::super::Result;

#[derive(Clone, Copy, Debug)]
pub(super) struct BlobMeta {
    pub frame_offset: u64,
    pub data_offset: u64,
    pub data_size: usize,
    pub kind: ElementKind,
    pub min_id: i64,
    pub max_id: i64,
    pub count: u64,
    pub has_tagindex: bool,
    pub has_tags: bool,
    /// A node blob holding both key families (`min_id < 0 < max_id`),
    /// decoded by the scan for its exact per-family spans
    /// ([`super::keys`]). `None` for every other blob.
    pub split: Option<BoundarySplit>,
}

/// One family's exact key range and node count inside a decoded blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FamilyPart {
    pub lo: u64,
    pub hi: u64,
    pub count: u64,
}

/// What decoding a node blob that holds both key families showed: each
/// family's part. Its claim reaches below zero, so the run is in the signed
/// arm, where the negative family is the IDs `<= 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BoundarySplit {
    pub neg: Option<FamilyPart>,
    pub pos: Option<FamilyPart>,
}

impl BlobMeta {
    /// The blob's indexdata claim, for holding it to the elements a stage
    /// decodes (`blob_meta::ClaimWalk`). The bbox is not kept here and no
    /// stage checks it.
    pub(super) fn claim(&self) -> crate::blob_meta::BlobIndex {
        crate::blob_meta::BlobIndex {
            kind: self.kind,
            min_id: self.min_id,
            max_id: self.max_id,
            count: self.count,
            bbox: None,
        }
    }
}

/// The order requirement the external join holds every blob it decodes to:
/// it writes blobs in input order under the input's sorted header, and its
/// stage 2 merge walk needs each node blob ascending.
pub(super) const EXTERNAL_ORDER: crate::blob_meta::Order = crate::blob_meta::Order::Required(
    "altw external: refusing an input whose blob contradicts its Sort.Type_then_ID header, \
     whose order the external join depends on",
);

/// Scan all OsmData blob headers once and retain only the metadata ALTW
/// external join actually reuses later. Walks via the pread-only
/// `HeaderWalker` so blob bodies stay out of the page cache during the
/// scan - stage 1 / stage 4 open their own fds and pread the bodies they
/// need.
///
/// This is the external join's one trust boundary for indexdata ranges,
/// so every range check lives here, located at the offending blob:
///
/// - a reversed range (`max_id < min_id`) is malformed indexdata;
/// - the ranges must follow canonical `Sort.Type_then_ID` order. Stage 2
///   partitions the node blobs by range with `partition_point` and walks
///   bucket records forward through them, so node blobs out of order or
///   overlapping turn present nodes into missing locations (zeroed, under
///   `--ignore-missing-nodes`); stage 4 also writes blobs in input order
///   under the input's sorted header. Equal boundaries pass, as in
///   `OrderWatermark`: duplicate ids are a separate concern. Indexdata
///   ranges are exact by contract (`check --index`), so ranges that break
///   the order mean the header's claim, or the indexdata, is false. That
///   includes an overstated `max_id` overlapping the next blob, which stage
///   2's walk alone would tolerate (it bounds each blob by its last decoded
///   id). Refusing it is deliberate: tolerating it while still catching
///   interleaved blobs would need each blob's true last id, a per-blob
///   decode in this header-only scan.
///
/// Header-level, with one exception: a node blob whose claim reaches both
/// below and above zero (`min_id < 0 < max_id`) is decoded here, once. Its
/// range names neither its first element (blobs `[-1]` then `[0, -2, 1]`
/// have in-order ranges) nor where its IDs `<= 0` end and its positive ones
/// start, which the join's key families need ([`super::keys`]). The decode
/// holds the whole blob to its claim, its kind and canonical order, and its
/// actual first and last elements go to the order watermark. A sorted file
/// has at most one such blob, and a file without negative IDs none.
/// Elsewhere the order of elements inside one blob is not visible here.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn scan_blob_metadata(
    input: &PinnedInput,
    parse_tagdata: bool,
    host: &crate::host::Entered<'_>,
) -> Result<Vec<BlobMeta>> {
    host.invocation.marker("ALTW_BLOB_META_SCAN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, host)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input.path()))?;

    let mut metas = Vec::new();
    let mut order = crate::osm_id::OrderWatermark::default();
    let mut boundary = BoundaryDecoder::default();
    let mut node_blobs: u64 = 0;
    let mut way_blobs: u64 = 0;
    let mut relation_blobs: u64 = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        let idx = meta.index.as_ref().ok_or_else(|| {
            walker.locate_meta(
                &meta,
                crate::Error::invalid_data(
                    "external join metadata scan: OsmData blob missing indexdata",
                ),
            )
        })?;
        let split = if matches!(idx.kind, ElementKind::Node) && idx.min_id < 0 && idx.max_id > 0 {
            let (split, first, last) = boundary
                .decode(input, meta.data_offset, meta.data_size, idx)
                .map_err(|e| walker.locate_meta(&meta, e))?;
            check_range(idx, Some((first, last)), &mut order)
                .map_err(|e| walker.locate_meta(&meta, e))?;
            Some(split)
        } else {
            check_range(idx, None, &mut order).map_err(|e| walker.locate_meta(&meta, e))?;
            None
        };
        let tag_index = if parse_tagdata {
            meta.tagdata
                .as_deref()
                .and_then(crate::blob_meta::TagIndex::deserialize)
        } else {
            None
        };
        let has_tagindex = tag_index.is_some();
        // An entirely empty stored key set is trusted as "no tagged
        // elements", and lets stage 4 drop the blob's untagged nodes without
        // decoding it. Accepted residue: pbfhogg 0.5.0 and earlier dropped an
        // explicit empty tag key from tagdata, so a legacy node blob whose
        // only tag key is "" carries this same encoding and loses its
        // empty-key-tagged nodes here. It cannot be told apart from the
        // scanner's legitimate zero-key tagdata for an untagged blob without
        // decoding the payload, which would forfeit the skip on every file;
        // `check --index` reports the omission and `cat --reindex` repairs it.
        let has_tags = if parse_tagdata {
            tag_index.is_none_or(|ti| !ti.keys_empty())
        } else {
            false
        };
        match idx.kind {
            ElementKind::Node => node_blobs += 1,
            ElementKind::Way => way_blobs += 1,
            ElementKind::Relation => relation_blobs += 1,
        }
        metas.push(BlobMeta {
            frame_offset: meta.frame_start,
            data_offset: meta.data_offset,
            data_size: meta.data_size,
            kind: idx.kind,
            min_id: idx.min_id,
            max_id: idx.max_id,
            count: idx.count,
            has_tagindex,
            has_tags,
            split,
        });
    }
    host.invocation.marker("ALTW_BLOB_META_SCAN_END");

    host.invocation.counter("altw_meta_node_blobs", node_blobs);
    host.invocation
        .counter("altw_meta_boundary_blobs_decoded", boundary.decoded);
    host.invocation.counter("altw_meta_way_blobs", way_blobs);
    host.invocation
        .counter("altw_meta_relation_blobs", relation_blobs);
    Ok(metas)
}

/// The range checks [`scan_blob_metadata`] documents, for one blob in file
/// order: not reversed, and in canonical order after the blobs before it.
/// `decoded` is a decoded blob's actual first and last IDs, which the order
/// check then uses instead of what the range implies. Unlocated: the caller
/// locates the error at the blob. Auto routing asks the same question of the
/// same claims through [`super::HeaderAdmission`], without the decode, so it
/// never routes here an input these checks refuse on its indexdata.
pub(super) fn check_range(
    idx: &crate::blob_meta::BlobIndex,
    decoded: Option<(i64, i64)>,
    order: &mut crate::osm_id::OrderWatermark,
) -> Result<()> {
    // Defensive: `BlobIndex::deserialize` already reads an inverted range as
    // no index, which the walk's partial-index refusal reaches first.
    if idx.max_id < idx.min_id {
        return Err(crate::error::new_indexdata_mismatch(format!(
            "altw external: {} blob has reversed indexdata range [min_id={}, max_id={}]",
            idx.kind.as_str(),
            idx.min_id,
            idx.max_id,
        )));
    }
    let observed = match decoded {
        Some((first, last)) => order.observe_keys(
            idx.kind.ordinal(),
            crate::osm_id::osm_id_key(first),
            crate::osm_id::osm_id_key(last),
        ),
        None => order.observe(idx.kind.ordinal(), idx.min_id, idx.max_id),
    };
    if let Err(v) = observed {
        return Err(crate::error::new_unsorted(format!(
            "altw external: indexdata ranges contradict the sorted header - a {} blob \
             holding id {} follows {} blobs already reaching id {}. \
             The external join needs Sort.Type_then_ID input with exact indexdata \
             ranges, and one of the two is false here.",
            idx.kind.as_str(),
            v.first_id,
            ElementKind::from_ordinal(v.previous_kind).map_or("unknown", ElementKind::as_str),
            v.previous_id,
        )));
    }
    Ok(())
}

/// Decodes the node blobs the scan must see inside, opening the input on
/// the first one, so an input without one opens nothing.
#[derive(Default)]
struct BoundaryDecoder {
    file: Option<std::fs::File>,
    read_buf: crate::read::read_buf::ReadBuf,
    decompressed: Vec<u8>,
    group_starts: Vec<(usize, usize)>,
    decoded: u64,
}

impl BoundaryDecoder {
    /// Decode one node blob holding both key families, held whole to its
    /// claim, kind and canonical order; returns its split and its actual
    /// first and last IDs.
    fn decode(
        &mut self,
        input: &PinnedInput,
        data_offset: u64,
        data_size: usize,
        claim: &crate::blob_meta::BlobIndex,
    ) -> Result<(BoundarySplit, i64, i64)> {
        let file = match &mut self.file {
            Some(f) => f,
            slot @ None => slot.insert(crate::read::input::InputSource::from(input).open()?),
        };
        let raw = self
            .read_buf
            .read_exact_at(&*file, data_offset, data_size)
            .map_err(|e| crate::Error::from(e).context("metadata scan pread"))?;
        crate::blob::decompress_blob_raw(raw, &mut self.decompressed)
            .map_err(|e| e.context("metadata scan decompress"))?;
        let mut walk = crate::blob_meta::ClaimWalk::new(Some(claim), EXTERNAL_ORDER);
        let mut walk_error = None;
        let mut last = 0_i64;
        let mut neg: Option<FamilyPart> = None;
        let mut pos: Option<FamilyPart> = None;
        let kinds = crate::scan::node::extract_node_coords(
            &self.decompressed,
            &mut self.group_starts,
            |id, _, _| {
                if walk_error.is_none()
                    && let Err(e) = walk.observe(id)
                {
                    walk_error = Some(e);
                }
                last = id;
                let part = if id > 0 { &mut pos } else { &mut neg };
                let key = id.unsigned_abs();
                match part {
                    Some(p) => {
                        p.lo = p.lo.min(key);
                        p.hi = p.hi.max(key);
                        p.count += 1;
                    }
                    None => {
                        *part = Some(FamilyPart {
                            lo: key,
                            hi: key,
                            count: 1,
                        });
                    }
                }
            },
        )
        .map_err(|e| e.context("metadata scan node extract"))?;
        kinds.refuse_under(Some(ElementKind::Node))?;
        if let Some(e) = walk_error {
            return Err(e);
        }
        let Some(first) = walk.finish()? else {
            return Err(crate::Error::internal(
                "an indexed node blob decoded to no element",
            ));
        };
        self.decoded += 1;
        Ok((BoundarySplit { neg, pos }, first, last))
    }
}
